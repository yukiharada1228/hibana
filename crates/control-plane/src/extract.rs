//! リクエストボディ抽出のプロジェクト共通ラッパ。
//!
//! axum の素の `Json<T>` は、不正な JSON / 不正な Content-Type / コメント入り
//! ボディ等で **デフォルトの `JsonRejection`**（`text/plain` ボディ、
//! `{ "error": { code, message, retryable } }` エンベロープ非準拠）を返す。
//! これは error.rs が保証する統一エンベロープを破り、ボディがハンドラに到達
//! したか否かで応答形状が変わる（§889 が禁じる shape-divergence）。
//!
//! `JsonBody<T>` は抽出失敗を `AppError`（= `FaasError::InvalidRequest`）へ
//! 写像し、他のエラーと同一のエンベロープでレンダリングする。メッセージは
//! 汎用固定文言とし、serde_json のパーサ詳細（オフセットやボディ断片）は
//! 一切漏らさない（§889 5xx/詳細漏洩防止と同方針）。

use axum::extract::rejection::JsonRejection;
use axum::extract::FromRequest;
use axum::http::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hibana_shared::FaasError;
use std::time::Duration;

use crate::error::AppError;

// An absolute reception deadline, not an idle timeout: trickling bytes must not
// keep an admitted management request (and the maintenance drain) alive forever.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound received AND decoded JSON before body extraction, across router clones.
/// Holding the permit through the handler also bounds bodies waiting on DB/auth.
/// Keep this outside authentication: login and bootstrap routes are public too.
pub(crate) async fn limit_json_requests(
    axum::extract::State(slots): axum::extract::State<std::sync::Arc<tokio::sync::Semaphore>>,
    req: Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let receives_json = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .is_some()
        && matches!(
            *req.method(),
            axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH
        )
        && req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(json_media_type);
    if !receives_json {
        return next.run(req).await;
    }
    let Ok(_permit) = slots.try_acquire_owned() else {
        return crate::admission::RateLimited::json_capacity().into_response();
    };
    next.run(req).await
}

// Conservative superset of Axum's application/json and application/*+json.
// Invalid parameters may consume a slot briefly, but valid JSON cannot bypass it.
fn json_media_type(value: &str) -> bool {
    let media = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    media == "application/json" || (media.starts_with("application/") && media.ends_with("+json"))
}

/// `Json<T>` 互換のリクエストボディ抽出器。抽出失敗を統一エンベロープへ写像する。
///
/// レスポンス生成には引き続き axum の `Json` を使う（本型は抽出専用）。
#[derive(Debug)]
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(
        req: Request<axum::body::Body>,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Only extraction is bounded. Once admitted, a management write must be
        // allowed to finish rather than being canceled mid-transaction.
        match tokio::time::timeout(RECEIVE_TIMEOUT, Json::<T>::from_request(req, state)).await {
            Ok(Ok(Json(value))) => Ok(JsonBody(value)),
            // Serde errors can quote a submitted Secret or credential. Record
            // only the category/status, including when debug logging is enabled.
            Ok(Err(rejection)) => {
                tracing::debug!(source_status = %rejection.status(), "rejected request body");
                Err(AppError::from(FaasError::InvalidRequest("invalid request body".into())).into_response())
            }
            Err(_) => Err((StatusCode::REQUEST_TIMEOUT, Json(serde_json::json!({"error": {
                "code": "request_timeout", "message": "request body reception timed out", "retryable": true
            }}))).into_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::FromRequest;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Dummy {
        #[allow(dead_code)]
        a: i32,
    }

    #[tokio::test]
    async fn invalid_json_values_are_never_logged_even_at_debug_level() {
        use std::sync::{Arc, Mutex};
        use tracing::instrument::WithSubscriber;
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(move || Capture(writer.clone()))
            .finish();
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"a":"fixture-private-secret"}"#))
            .unwrap();
        let rejected = JsonBody::<Dummy>::from_request(request, &())
            .with_subscriber(subscriber)
            .await
            .unwrap_err();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("rejected request body"),
            "captured logs: {logs}"
        );
        assert!(!logs.contains("fixture-private-secret"));
    }

    #[tokio::test]
    async fn json_media_types_cannot_bypass_reception_capacity() {
        for content_type in [
            "application/json",
            "application/json; charset=utf-8",
            "APPLICATION/JSON",
            "application/problem+json",
            "application/cloudevents+json; charset=utf-8",
        ] {
            let request = Request::builder()
                .header(header::CONTENT_TYPE, content_type)
                .body(axum::body::Body::from("{\"a\":1}"))
                .unwrap();
            assert!(
                JsonBody::<Dummy>::from_request(request, &()).await.is_ok(),
                "{content_type}"
            );
            assert!(json_media_type(content_type));
        }
        assert!(!json_media_type("multipart/form-data; boundary=example"));
        assert!(!json_media_type("text/plain"));
    }

    #[tokio::test]
    async fn json_capacity_is_shared_and_held_until_the_handler_finishes() {
        use axum::{
            routing::{get, post},
            Router,
        };
        use std::sync::Arc;
        use tokio::{
            io::AsyncWriteExt,
            sync::{Notify, Semaphore},
        };

        let slots = Arc::new(Semaphore::new(1));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handler = {
            let entered = entered.clone();
            let release = release.clone();
            move |JsonBody(_): JsonBody<Dummy>| async move {
                entered.notify_one();
                release.notified().await;
                StatusCode::NO_CONTENT
            }
        };
        let app = Router::new()
            .route("/json", post(handler))
            .route("/healthz", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(
                slots.clone(),
                limit_json_requests,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let request = client
            .post(format!("http://{address}/json"))
            .json(&serde_json::json!({"a": 1}));
        let first = tokio::spawn(async move { request.send().await.unwrap() });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            slots.available_permits(),
            0,
            "decoded JSON still occupies a slot"
        );
        let response = client
            .post(format!("http://{address}/json"))
            .header(header::CONTENT_TYPE, "application/problem+json")
            .body(reqwest::Body::wrap_stream(futures::stream::pending::<
                Result<axum::body::Bytes, std::io::Error>,
            >()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
            "json_capacity"
        );
        assert_eq!(
            client
                .get(format!("http://{address}/healthz"))
                .header(header::CONTENT_TYPE, "application/json")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        release.notify_one();
        assert_eq!(first.await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(
            client
                .post(format!("http://{address}/json"))
                .header(header::CONTENT_TYPE, "application/json")
                .body("{")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            slots.available_permits(),
            1,
            "malformed JSON releases capacity"
        );

        let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
        connection.write_all(b"POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while slots.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(connection);
        tokio::time::timeout(Duration::from_secs(3), async {
            while slots.available_permits() != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("disconnect must release JSON capacity");
        server.abort();
        let _ = server.await;
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_and_trickling_json_have_an_absolute_deadline() {
        use axum::body::{Body, Bytes};
        for trickle in [false, true] {
            let body = Body::from_stream(futures::stream::unfold((), move |()| async move {
                if trickle {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from_static(b" ")), ()))
                } else {
                    std::future::pending().await
                }
            }));
            let request = Request::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .unwrap();
            let started = tokio::time::Instant::now();
            let response = JsonBody::<Dummy>::from_request(request, &())
                .await
                .unwrap_err();
            assert_eq!(started.elapsed(), RECEIVE_TIMEOUT);
            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                serde_json::json!({
                    "error": {"code": "request_timeout", "message": "request body reception timed out", "retryable": true}
                })
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn complete_json_is_accepted_before_the_deadline() {
        let body = axum::body::Body::from_stream(futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(9)).await;
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{\"a\":42}"))
        }));
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap();
        assert_eq!(
            JsonBody::<Dummy>::from_request(request, &())
                .await
                .unwrap()
                .0
                .a,
            42
        );
    }

    /// 抽出失敗は統一エンベロープ（code/message/retryable）の 400 にレンダリングされ、
    /// パーサ詳細（オフセット等）を漏らさない。DB 非依存（純粋な抽出パス）。
    async fn reject(
        body: &'static [u8],
        content_type: Option<&'static str>,
    ) -> (StatusCode, String) {
        let mut builder = axum::http::Request::builder().method("POST").uri("/");
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        let req = builder.body(axum::body::Body::from(body)).unwrap();
        let err = JsonBody::<Dummy>::from_request(req, &())
            .await
            .expect_err("malformed body must be rejected");
        let resp = err.into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn malformed_json_maps_to_envelope_400() {
        let (status, body) = reject(b"{ not json", Some("application/json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let obj = v["error"].as_object().expect("error envelope object");
        assert_eq!(obj.len(), 3);
        assert_eq!(obj["code"], "invalid_request");
        // message は汎用固定文言（FaasError::InvalidRequest の Display 接頭辞付き）。
        assert_eq!(obj["message"], "invalid request: invalid request body");
        assert_eq!(obj["retryable"], false);
    }

    #[tokio::test]
    async fn wrong_content_type_maps_to_envelope_400() {
        // text/plain（JSON でない Content-Type）も統一エンベロープへ。
        let (status, body) = reject(b"{\"a\":1}", Some("text/plain")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], "invalid_request");
        assert_eq!(
            v["error"]["message"],
            "invalid request: invalid request body"
        );
    }

    #[tokio::test]
    async fn rejection_does_not_leak_parser_detail() {
        // serde_json のオフセットやボディ断片が漏れていないこと。
        let (_status, body) =
            reject(b"{\"a\": \"oops-secret-value\"}", Some("application/json")).await;
        assert!(!body.contains("oops-secret-value"));
        assert!(!body.contains("column"));
        assert!(!body.contains("line"));
    }
}
