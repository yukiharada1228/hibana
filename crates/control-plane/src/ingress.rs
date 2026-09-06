//! M11 (§4.2): 公開 HTTP ingress gateway。
//!
//! `<app>.<tenant>.<base>` の Host からテナント/アプリを解決し、**明示的に
//! `ingress_enabled` を立てた** component だけを invoke へ変換して HTTP 応答を返す。
//! deny-by-default（未 opt-in / 版なし / 不明 host は存在を晒さず 404）。
//!
//! これは axum の `fallback` として動く。API ルートにマッチしなかったリクエストのうち、
//! Host が ingress ベースドメイン配下のものだけを gateway として処理し、それ以外は 404。
//!
//! **境界**: これは *ingress*（外→関数）のみ。関数からの *egress* は M9c の allowlist /
//! socket_addr_check が唯一の経路で、ここは一切関与しない。呼び出しは通常の invoke と同じ
//! admission / 計量 / provenance パスを通る（tenant コンテキストで実行）。

use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::auth::Principal;
use crate::extract::JsonBody;
use crate::handlers::{self, InvokeQuery, InvokeRequest};
use crate::state::AppState;
use faas_shared::{b64url_decode, b64url_encode, Role, Scope};

/// リクエスト/レスポンス body の上限（gateway 経路）。
const MAX_INGRESS_BODY: usize = 8 * 1024 * 1024;
/// cold precompile を吸収するための実行完了ポーリング上限。JS/Hono component は
/// 十数 MiB で初回 precompile が長い（`hibana deploy` が warm-up する前提だが、
/// 未 warm な初回公開ヒットにも余裕を持たせる）。
const POLL_DEADLINE: Duration = Duration::from_millis(45_000);
/// ポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 転送時にコピーしない応答ヘッダ（長さ/接続管理は hyper に任せる）。
fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length" | "transfer-encoding" | "connection" | "keep-alive"
    )
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, msg.to_string()).into_response()
}

/// Host（`app.tenant.base[:port]`, 小文字化済み）から `(app, tenant)` を取り出す。
/// ちょうど `app.tenant.<base>` の形（app / tenant は各 1 ラベル）でなければ `None`。
fn split_host<'a>(host: &'a str, base: &str) -> Option<(&'a str, &'a str)> {
    let host = host.split(':').next().unwrap_or(host);
    let rest = host.strip_suffix(base)?.strip_suffix('.')?; // "app.tenant"
    let (app, tenant) = rest.rsplit_once('.')?;
    // app / tenant は単一ラベル（余分な '.' はサブドメイン多段 = 非対応 → 404）。
    if app.is_empty() || tenant.is_empty() || app.contains('.') {
        return None;
    }
    Some((app, tenant))
}

/// axum fallback: ingress host なら gateway 処理、それ以外は 404。
pub async fn ingress_fallback(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let Some(base) = state.ingress_base_domain() else {
        // gateway 無効（INGRESS_BASE_DOMAIN 未設定）。
        return not_found();
    };
    let base = base.to_string();

    let (parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some((app, tenant_slug)) = split_host(&host, &base) else {
        return not_found();
    };

    // 1) tenant_slug -> tenant_id（GUC 不要の SECURITY DEFINER 経路。login と同じ）。
    let tenant_id = match db_find_tenant(&state, tenant_slug).await {
        Some(t) => t,
        None => return not_found(),
    };

    // 2) component 解決 + deny-by-default gate（GUC 下）。
    let comp = match db_find_component(&state, &tenant_id, app).await {
        Some(c) => c,
        None => return not_found(),
    };
    if !comp.ingress_enabled || comp.active_version_id.is_none() {
        // 未 opt-in / active 版なしは「存在しない」と同じ扱い（情報を晒さない）。
        return not_found();
    }

    // 3) 実リクエスト -> HTTP エンベロープ JSON。
    let body_bytes = match axum::body::to_bytes(body, MAX_INGRESS_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    let envelope = build_envelope(&parts, &body_bytes);

    // 4) 合成 principal で通常 invoke へ合流（admission / 計量 / provenance を共有）。
    let principal = Principal {
        tenant_id: tenant_id.clone(),
        user_id: None,
        token_id: "ingress".to_string(),
        scopes: vec![Scope::Invoke],
        role: Role::Admin,
    };
    let invoke_req = InvokeRequest {
        component: app.to_string(),
        input: Value::Object(envelope),
        input_ref: None,
        execution_id: None,
    };
    let invoke_resp = handlers::invoke(
        State(state.clone()),
        principal,
        Query(InvokeQuery {
            wait: Some("1".to_string()),
        }),
        HeaderMap::new(),
        JsonBody(invoke_req),
    )
    .await;

    let resp = match invoke_resp {
        Ok(r) => r,
        // admission 429 / 不正リクエスト等は invoke の写像をそのまま返す。
        Err(e) => return e.into_response(),
    };

    // 5) invoke 応答（sync 200 か 202 pending）を読み、pending なら実行完了までポーリング。
    let peek = read_peek(resp).await;
    let (status, output, error) = resolve_terminal(&state, &tenant_id, peek).await;

    match status.as_str() {
        "succeeded" => envelope_to_http(output),
        "" | "pending" | "running" => {
            // 期限内に終端に達しなかった（超 cold / 詰まり）。
            (
                StatusCode::GATEWAY_TIMEOUT,
                "function did not complete in time",
            )
                .into_response()
        }
        _ => {
            // failed / timeout。
            let msg = error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("execution {status}"));
            bad_gateway(&msg)
        }
    }
}

/// リクエストの各パーツを HTTP エンベロープ JSON（adapter が読む規約）へ変換する。
fn build_envelope(parts: &axum::http::request::Parts, body_bytes: &[u8]) -> Map<String, Value> {
    let mut headers = Map::new();
    for (k, v) in parts.headers.iter() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_string(), Value::String(s.to_string()));
        }
    }

    let mut env = Map::new();
    env.insert(
        "method".into(),
        Value::String(parts.method.as_str().to_string()),
    );
    env.insert("path".into(), Value::String(parts.uri.path().to_string()));
    if let Some(q) = parts.uri.query() {
        env.insert("query".into(), Value::String(format!("?{q}")));
    }
    env.insert("headers".into(), Value::Object(headers));

    if !body_bytes.is_empty() {
        // UTF-8 ならそのまま文字列、バイナリなら base64url（Rust/JS 同一スキーム）。
        match std::str::from_utf8(body_bytes) {
            Ok(s) => {
                env.insert("body".into(), Value::String(s.to_string()));
                env.insert("bodyBase64".into(), Value::Bool(false));
            }
            Err(_) => {
                env.insert("body".into(), Value::String(b64url_encode(body_bytes)));
                env.insert("bodyBase64".into(), Value::Bool(true));
            }
        }
    }
    env
}

/// invoke の応答 body から `{execution_id, status, output, error}` を取り出す。
struct Peek {
    execution_id: Option<String>,
    status: String,
    output: Option<Value>,
    error: Option<Value>,
}

async fn read_peek(resp: Response) -> Peek {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        execution_id: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        output: Option<Value>,
        #[serde(default)]
        error: Option<Value>,
    }
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_INGRESS_BODY)
        .await
        .unwrap_or_default();
    let raw: Raw = serde_json::from_slice(&bytes).unwrap_or(Raw {
        execution_id: None,
        status: None,
        output: None,
        error: None,
    });
    Peek {
        execution_id: raw.execution_id,
        status: raw.status.unwrap_or_default(),
        output: raw.output,
        error: raw.error,
    }
}

/// pending なら実行行を終端までポーリングして最終 (status, output, error) を返す。
async fn resolve_terminal(
    state: &AppState,
    tenant_id: &str,
    peek: Peek,
) -> (String, Option<Value>, Option<Value>) {
    let terminal = |s: &str| matches!(s, "succeeded" | "failed" | "timeout");
    if terminal(&peek.status) {
        return (peek.status, peek.output, peek.error);
    }
    let Some(eid) = peek.execution_id else {
        return (peek.status, peek.output, peek.error);
    };

    let start = Instant::now();
    let mut last = (peek.status, peek.output, peek.error);
    while start.elapsed() < POLL_DEADLINE {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Some(row) = db_get_execution(state, tenant_id, &eid).await {
            let done = terminal(&row.status);
            last = (row.status, row.output, row.error);
            if done {
                break;
            }
        }
    }
    last
}

/// adapter の Response エンベロープ（`{status, headers, body, bodyBase64}`）を
/// 実 HTTP 応答へ写す。エンベロープでない場合は 200 + JSON 素通し。
fn envelope_to_http(output: Option<Value>) -> Response {
    let out = output.unwrap_or(Value::Null);
    if let Some(obj) = out.as_object() {
        if let Some(status_code) = obj.get("status").and_then(|v| v.as_u64()) {
            let code = StatusCode::from_u16(status_code as u16).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder().status(code);
            if let Some(hs) = obj.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in hs {
                    if is_hop_header(k) {
                        continue;
                    }
                    if let Some(vs) = v.as_str() {
                        if let (Ok(name), Ok(val)) = (
                            HeaderName::from_bytes(k.as_bytes()),
                            HeaderValue::from_str(vs),
                        ) {
                            builder = builder.header(name, val);
                        }
                    }
                }
            }
            let b64 = obj
                .get("bodyBase64")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let bytes: Vec<u8> = match obj.get("body") {
                Some(Value::String(s)) if b64 => b64url_decode(s).unwrap_or_default(),
                Some(Value::String(s)) => s.clone().into_bytes(),
                _ => Vec::new(),
            };
            return builder
                .body(Body::from(bytes))
                .unwrap_or_else(|_| bad_gateway("malformed response envelope"));
        }
    }
    // エンベロープ形でない出力（非 Hono component）は JSON でそのまま返す。
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&out).unwrap_or_default(),
    )
        .into_response()
}

// --- DB ラッパ（GUC 境界をここに閉じる） ------------------------------------

async fn db_find_tenant(state: &AppState, slug: &str) -> Option<String> {
    crate::db::find_tenant_id_by_slug(state.pool(), slug)
        .await
        .ok()
        .flatten()
}

async fn db_find_component(
    state: &AppState,
    tenant_id: &str,
    name: &str,
) -> Option<crate::db::IngressComponentRow> {
    let mut tx = state.pool().begin().await.ok()?;
    if crate::db::set_tenant_guc(&mut tx, tenant_id).await.is_err() {
        return None;
    }
    let row = crate::db::find_ingress_component(&mut *tx, tenant_id, name)
        .await
        .ok()
        .flatten();
    let _ = tx.commit().await;
    row
}

async fn db_get_execution(
    state: &AppState,
    tenant_id: &str,
    execution_id: &str,
) -> Option<crate::db::ExecutionRow> {
    let mut tx = state.pool().begin().await.ok()?;
    if crate::db::set_tenant_guc(&mut tx, tenant_id).await.is_err() {
        return None;
    }
    let row = crate::db::get_execution(&mut *tx, tenant_id, execution_id)
        .await
        .ok()
        .flatten();
    let _ = tx.commit().await;
    row
}

#[cfg(test)]
mod tests {
    use super::split_host;

    #[test]
    fn splits_app_tenant_base() {
        assert_eq!(
            split_host("my-api.smoke.hibana.local", "hibana.local"),
            Some(("my-api", "smoke"))
        );
    }

    #[test]
    fn strips_port() {
        assert_eq!(
            split_host("my-api.smoke.hibana.local:8080", "hibana.local"),
            Some(("my-api", "smoke"))
        );
    }

    #[test]
    fn rejects_wrong_base() {
        assert_eq!(split_host("my-api.smoke.evil.com", "hibana.local"), None);
    }

    #[test]
    fn rejects_bare_base() {
        assert_eq!(split_host("hibana.local", "hibana.local"), None);
    }

    #[test]
    fn rejects_extra_labels() {
        // app が多段（a.b.smoke）は非対応。
        assert_eq!(split_host("a.b.smoke.hibana.local", "hibana.local"), None);
    }

    #[test]
    fn rejects_missing_tenant() {
        assert_eq!(split_host("app.hibana.local", "hibana.local"), None);
    }
}
