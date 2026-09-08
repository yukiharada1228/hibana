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
use axum::Json;
use hibana_shared::FaasError;

use crate::error::AppError;

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
    type Rejection = AppError;

    async fn from_request(
        req: Request<axum::body::Body>,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            // 詳細（オフセット・ボディ断片）はログにのみ残し、クライアントへは汎用文言。
            Err(rejection) => {
                tracing::debug!(error = %rejection, "rejected request body");
                Err(FaasError::InvalidRequest("invalid request body".into()).into())
            }
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
