//! Bounded multipart reception. No database transaction is held while receiving.
use super::{deployment::VersionEnvironment, AppError};
use axum::{
    extract::{multipart::Field, Multipart},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hibana_shared::{FaasError, ResourceLimits};
use serde_json::json;
use std::time::Duration;

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_METADATA_BYTES: usize = 256 * 1024;

pub(super) struct Upload {
    pub version: String,
    pub wasm_bytes: Vec<u8>,
    pub signature: Option<String>,
    pub resource_limits: ResourceLimits,
    pub activate: bool,
    pub ingress: Option<bool>,
    pub environment: VersionEnvironment,
}

pub(super) async fn receive(multipart: Multipart, max_bytes: u64) -> Result<Upload, Box<Response>> {
    match tokio::time::timeout(RECEIVE_TIMEOUT, parse(multipart, max_bytes)).await {
        Ok(result) => result.map_err(|error| Box::new(error.into_response())),
        Err(_) => Err(Box::new((
            StatusCode::REQUEST_TIMEOUT,
            Json(json!({"error":{
                "code":"upload_timeout", "message":"upload reception timed out", "retryable":true
            }})),
        )
            .into_response())),
    }
}

async fn parse(mut multipart: Multipart, max_bytes: u64) -> Result<Upload, AppError> {
    // --- multipart フィールドを収集 ---
    let mut version: Option<String> = None;
    let mut wasm_bytes: Option<Vec<u8>> = None;
    // M9a: 本体 sha256 に対する detached Ed25519 署名（base64url）。任意フィールド。
    let mut signature: Option<String> = None;
    let mut resource_limits: ResourceLimits = ResourceLimits::default();
    let mut activate = true;
    let mut ingress: Option<bool> = None;
    let mut environment = super::deployment::VersionEnvironment::default();
    let mut fields = std::collections::BTreeSet::new();

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| FaasError::InvalidRequest(format!("malformed multipart: {e}")))?
    {
        // Only eight named fields are supported; rejecting duplicates bounds their count.
        if let Some(name) = field.name() {
            if !fields.insert(name.to_owned()) {
                return Err(FaasError::InvalidRequest("duplicate multipart field".into()).into());
            }
        }
        match field.name() {
            Some("version") => {
                let v = text(field).await?;
                version = Some(v);
            }
            Some("resource_limits") => {
                let text = text(field).await?;
                resource_limits = serde_json::from_str(&text).map_err(|e| {
                    FaasError::InvalidRequest(format!("resource_limits is not valid JSON: {e}"))
                })?;
                // M4b (§4.3): §4.3 表の上限を超える値（max_memory > 1 GiB / max_wall_time > 30s /
                // max_execution_time > 60s）や論理不整合（max_execution_time < max_wall_time、
                // 各ゼロ値、max_fuel = Some(0)）はここで 422 で拒否する。worker 側の防御もあるが、
                // 受付時点で明示的に弾くことで「保存だけされて毎 invoke で落ちる」を防ぐ。
                resource_limits.validate()?;
            }
            Some("wasm") => {
                // (1) ストリーミング中にサイズ上限を強制。超過は即打ち切り。
                let mut buf: Vec<u8> = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("error reading wasm field: {e}"))
                })? {
                    if buf.len() as u64 + chunk.len() as u64 > max_bytes {
                        return Err(FaasError::InvalidRequest(format!(
                            "wasm exceeds max upload size of {max_bytes} bytes"
                        ))
                        .into());
                    }
                    buf.extend_from_slice(&chunk);
                }
                wasm_bytes = Some(buf);
            }
            Some("activate" | "ingress" | "vars" | "secrets") => {
                let name = field.name().unwrap().to_owned();
                let text = text(field).await?;
                // Do not include JSON parser errors: they can quote user-supplied values.
                let invalid = || FaasError::InvalidRequest(format!("invalid {name} field"));
                match name.as_str() {
                    "activate" => activate = serde_json::from_str(&text).map_err(|_| invalid())?,
                    "ingress" => {
                        ingress = Some(serde_json::from_str(&text).map_err(|_| invalid())?)
                    }
                    "vars" => {
                        environment.vars = serde_json::from_str(&text).map_err(|_| invalid())?
                    }
                    "secrets" => {
                        environment.secrets = serde_json::from_str(&text).map_err(|_| invalid())?
                    }
                    _ => unreachable!(),
                }
            }
            Some("signature") => {
                // M9a: 本体 sha256 に対する detached Ed25519 署名（base64url）。
                let v = text(field).await?;
                let v = v.trim().to_string();
                if !v.is_empty() {
                    signature = Some(v);
                }
            }
            _ => {
                return Err(FaasError::InvalidRequest("unsupported multipart field".into()).into());
            }
        }
    }

    let version = version
        .ok_or_else(|| FaasError::InvalidRequest("missing required field 'version'".into()))?;
    // Names remain human-readable labels. Reject URL dot segments, separators,
    // control characters and oversized database index keys before validation/I/O.
    if version.len() > 128
        || !version
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        return Err(FaasError::InvalidRequest(
            "version must be 1..=128 ASCII characters, start with a letter or digit, and contain only letters, digits, '.', '_', '+', '-'".into(),
        ).into());
    }
    let wasm_bytes = wasm_bytes
        .filter(|b| !b.is_empty())
        .ok_or_else(|| FaasError::InvalidRequest("missing required field 'wasm'".into()))?;

    Ok(Upload {
        version,
        wasm_bytes,
        signature,
        resource_limits,
        activate,
        ingress,
        environment,
    })
}

async fn text(mut field: Field<'_>) -> Result<String, AppError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| FaasError::InvalidRequest("invalid multipart field".into()))?
    {
        if bytes.len() + chunk.len() > MAX_METADATA_BYTES {
            return Err(
                FaasError::InvalidRequest("multipart metadata exceeds 256 KiB".into()).into(),
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| FaasError::InvalidRequest("multipart metadata must be UTF-8".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        extract::FromRequest,
        http::Request,
    };
    use futures::StreamExt;
    use serde_json::Value;

    async fn multipart(body: Body) -> Multipart {
        Multipart::from_request(
            Request::builder()
                .header("content-type", "multipart/form-data; boundary=fixture")
                .body(body)
                .unwrap(),
            &(),
        )
        .await
        .unwrap()
    }
    fn field(name: &str, value: &str) -> String {
        format!("--fixture\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
    }

    #[tokio::test]
    async fn unsupported_fields_are_rejected_without_echoing_values() {
        for name in ["capabilities", "resource_limit", "env", ""] {
            let body = field(name, r#"{"env":["PRIVATE_FIXTURE"]}"#)
                + &field("version", "1")
                + &field("wasm", "wasm")
                + "--fixture--\r\n";
            let Err(response) = receive(multipart(Body::from(body)).await, 4).await else {
                panic!("unsupported field accepted: {name:?}");
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body = std::str::from_utf8(&body).unwrap();
            assert!(body.contains("unsupported multipart field"));
            assert!(!body.contains("PRIVATE_FIXTURE"));
        }
    }

    #[tokio::test]
    async fn version_names_are_bounded_url_safe_labels() {
        for version in ["1", "v1.2.3-rc.1+build_7", &"a".repeat(128)] {
            let body = field("version", version) + &field("wasm", "wasm") + "--fixture--\r\n";
            let upload = receive(multipart(Body::from(body)).await, 4)
                .await
                .unwrap_or_else(|_| panic!("valid version rejected"));
            assert_eq!(upload.version, version);
        }
        for version in [
            "",
            ".",
            "..",
            "../v1",
            "a/b",
            "a\\b",
            "%2e",
            "a?b",
            "a#b",
            "a b",
            " a",
            "a\n",
            "a\r",
            "版1",
            &"a".repeat(129),
        ] {
            let body = field("version", version) + &field("wasm", "wasm") + "--fixture--\r\n";
            let Err(response) = receive(multipart(Body::from(body)).await, 4).await else {
                panic!("invalid version accepted: {version:?}");
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_uploads_have_one_absolute_deadline() {
        let partial = field("version", "1") + &field("wasm", "partial");
        let stream = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(partial))])
            .chain(futures::stream::pending());
        let started = tokio::time::Instant::now();
        let Err(response) = receive(multipart(Body::from_stream(stream)).await, 1024).await else {
            panic!("partial upload accepted")
        };
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(started.elapsed(), RECEIVE_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
            "upload_timeout"
        );
    }

    #[tokio::test]
    async fn complete_uploads_are_bounded_and_malformed_fields_are_rejected() {
        let valid = field("version", "1") + &field("wasm", "wasm");
        let complete = valid.clone() + "--fixture--\r\n";
        let upload = receive(multipart(Body::from(complete.clone())).await, 4)
            .await
            .unwrap_or_else(|_| panic!("valid upload rejected"));
        assert_eq!(upload.version, "1");
        assert_eq!(upload.wasm_bytes, b"wasm");
        assert!(receive(multipart(Body::from(complete)).await, 3)
            .await
            .is_err());
        for invalid in [
            valid.clone() + &field("version", "duplicate") + "--fixture--\r\n",
            valid.clone() + &field("vars", &"x".repeat(MAX_METADATA_BYTES + 1)) + "--fixture--\r\n",
            valid
                + "--fixture\r\nContent-Disposition: form-data; name=\"signature\"\r\n\r\ntruncated",
        ] {
            let Err(response) = receive(multipart(Body::from(invalid)).await, 4).await else {
                panic!("invalid multipart accepted")
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}
