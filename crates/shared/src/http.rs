//! Language-neutral HTTP request stored by the Control Plane and consumed by the runtime.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Internal transport marker. Guest responses must never set this header.
pub const WORKER_REJECTED_HEADER: &str = "x-hibana-worker-rejected";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    #[serde(rename = "bodyBase64")]
    pub body_base64: bool,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: BTreeMap::new(),
            body: String::new(),
            body_base64: false,
        }
    }
}

impl HttpRequest {
    pub fn from_parts(parts: &::http::request::Parts, body: &[u8]) -> Self {
        let (body, body_base64) = match std::str::from_utf8(body) {
            Ok(text) => (text.to_owned(), false),
            Err(_) => (crate::b64url_encode(body), true),
        };
        Self {
            method: parts.method.as_str().into(),
            path: parts.uri.path().into(),
            query: parts
                .uri
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default(),
            headers: parts
                .headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().into(), value.into()))
                })
                .collect(),
            body,
            body_base64,
        }
    }

    pub fn body_bytes(&self) -> Result<Vec<u8>, &'static str> {
        if self.body_base64 {
            crate::b64url_decode(&self.body).ok_or("invalid base64 HTTP body")
        } else {
            Ok(self.body.as_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_binary_and_text_requests_across_the_wire() {
        for body in [
            b"hello".as_slice(),
            &[0, 255, 128, 10],
            "雪".as_bytes(),
            &[],
        ] {
            let (parts, ()) = ::http::Request::builder()
                .method("POST")
                .uri("/echo?a=%2F")
                .header("content-type", "application/octet-stream")
                .body(())
                .unwrap()
                .into_parts();
            let input = HttpRequest::from_parts(&parts, body);
            let decoded: HttpRequest =
                serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
            assert_eq!(decoded.body_bytes().unwrap(), body);
            assert_eq!(decoded.method, "POST");
            assert_eq!(decoded.path, "/echo");
            assert_eq!(decoded.query, "?a=%2F");
            assert_eq!(decoded.headers["content-type"], "application/octet-stream");
        }
    }
    #[test]
    fn reads_existing_envelopes_and_rejects_corrupt_encoded_bodies() {
        let request: HttpRequest =
            serde_json::from_str(r#"{"method":"GET","path":"/old","headers":{}}"#).unwrap();
        assert!(request.body_bytes().unwrap().is_empty());
        let corrupt: HttpRequest =
            serde_json::from_str(r#"{"body":"%invalid","bodyBase64":true}"#).unwrap();
        assert!(corrupt.body_bytes().is_err());
    }
}
