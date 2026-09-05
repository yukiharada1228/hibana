//! M13: R2 バインディングの内部エンドポイント（worker → CP → MinIO）。
//!
//! worker は keyless by design（S3 資格情報を持たない）。R2 の実 I/O は CP が代行する。
//! **公開 listener には出さない**（internal listener 127.0.0.1:8081 のみ）。認証は job_token の
//! Ed25519 署名で、テナントはトークンの claim（CP 発行）から取る —— worker の主張ではない。
//! S3 キーは `r2/{tenant}/{bucket}/{key}` に固定するので、guest が bucket/key を自由に指定しても
//! **自テナントの prefix 外には出られない**（tenant は署名トークン由来）。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;

use crate::error::AppError;
use crate::state::AppState;
use faas_shared::FaasError;

const JOB_TOKEN_HEADER: &str = "x-hibana-job-token";

/// job_token を検証してテナントを得る（claim 由来。exp + テナント停止も確認）。
/// M15: queue の内部エンドポイントでも再利用するため pub(crate)。
pub(crate) async fn tenant_from_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, AppError> {
    let token = headers
        .get(JOB_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(FaasError::Unauthorized)?;
    let claims = state
        .signer()
        .verifier()
        .verify(token)
        .map_err(|_| FaasError::Unauthorized)?;
    let now = chrono::Utc::now().timestamp();
    if claims.exp <= now {
        return Err(FaasError::Unauthorized.into());
    }
    if !crate::db::tenant_is_active(state.pool(), &claims.tenant_id).await? {
        return Err(FaasError::Forbidden.into());
    }
    Ok(claims.tenant_id)
}

fn require_header(headers: &HeaderMap, name: &str) -> Result<String, AppError> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FaasError::InvalidRequest(format!("missing header {name}")).into())
}

/// `r2/{tenant}/{bucket}/{key}` を組む。
fn object_key(tenant: &str, bucket: &str, key: &str) -> String {
    format!("r2/{tenant}/{bucket}/{key}")
}

fn decode_meta(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .get("x-r2-meta")
        .and_then(|v| v.to_str().ok())
        .and_then(faas_shared::b64url_decode)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn encode_meta(meta: &HashMap<String, String>) -> String {
    let obj: serde_json::Map<String, serde_json::Value> = meta
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    faas_shared::b64url_encode(serde_json::Value::Object(obj).to_string().as_bytes())
}

/// PUT /internal/r2/object — オブジェクト保存（本体 = リクエストボディ）。
pub async fn put_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let tenant = tenant_from_token(&state, &headers).await?;
    let bucket = require_header(&headers, "x-r2-bucket")?;
    let key = require_header(&headers, "x-r2-key")?;
    let content_type = headers
        .get("x-r2-content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let meta = decode_meta(&headers);

    let m = state
        .storage()
        .r2_put(
            &object_key(&tenant, &bucket, &key),
            body.to_vec(),
            content_type.as_deref(),
            meta,
        )
        .await?;

    // メタは get/head と同じくヘッダで返す（worker が一律に guest へ写せる）。
    Ok((
        StatusCode::OK,
        meta_headers(&key, m.size, &m.etag, &m.content_type, &m.metadata),
    )
        .into_response())
}

fn meta_headers(
    key: &str,
    size: i64,
    etag: &str,
    content_type: &Option<String>,
    meta: &HashMap<String, String>,
) -> HeaderMap {
    let mut hm = HeaderMap::new();
    let mut put = |name: &'static str, val: String| {
        if let Ok(v) = HeaderValue::from_str(&val) {
            hm.insert(HeaderName::from_static(name), v);
        }
    };
    put("x-r2-key", key.to_string());
    put("x-r2-size", size.to_string());
    put("x-r2-etag", etag.to_string());
    put("x-r2-meta", encode_meta(meta));
    if let Some(ct) = content_type {
        put("x-r2-content-type", ct.clone());
    }
    hm
}

/// GET /internal/r2/object — 取得（`x-r2-head: 1` でメタのみ）。
pub async fn get_object(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let tenant = tenant_from_token(&state, &headers).await?;
    let bucket = require_header(&headers, "x-r2-bucket")?;
    let key = require_header(&headers, "x-r2-key")?;
    let full = object_key(&tenant, &bucket, &key);
    let head_only = headers.get("x-r2-head").and_then(|v| v.to_str().ok()) == Some("1");

    if head_only {
        match state.storage().r2_head(&full).await? {
            None => Ok(StatusCode::NOT_FOUND.into_response()),
            Some(m) => Ok((
                StatusCode::OK,
                meta_headers(&key, m.size, &m.etag, &m.content_type, &m.metadata),
            )
                .into_response()),
        }
    } else {
        match state.storage().r2_get(&full).await? {
            None => Ok(StatusCode::NOT_FOUND.into_response()),
            Some(o) => Ok((
                StatusCode::OK,
                meta_headers(&key, o.meta.size, &o.meta.etag, &o.meta.content_type, &o.meta.metadata),
                o.bytes,
            )
                .into_response()),
        }
    }
}

/// DELETE /internal/r2/object — 削除。
pub async fn delete_object(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let tenant = tenant_from_token(&state, &headers).await?;
    let bucket = require_header(&headers, "x-r2-bucket")?;
    let key = require_header(&headers, "x-r2-key")?;
    state
        .storage()
        .r2_delete(&object_key(&tenant, &bucket, &key))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// GET /internal/r2/list — prefix 一覧。
pub async fn list_objects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let tenant = tenant_from_token(&state, &headers).await?;
    let bucket = require_header(&headers, "x-r2-bucket")?;
    let prefix = headers
        .get("x-r2-prefix")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let limit: i32 = headers
        .get("x-r2-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let strip = format!("r2/{tenant}/{bucket}/");
    let key_prefix = format!("{strip}{prefix}");
    let items = state
        .storage()
        .r2_list(&key_prefix, &strip, limit)
        .await?;
    let objs: Vec<serde_json::Value> = items
        .iter()
        .map(|o| serde_json::json!({ "key": o.key, "size": o.size, "etag": o.etag }))
        .collect();
    Ok((StatusCode::OK, axum::Json(serde_json::json!({ "objects": objs }))).into_response())
}
