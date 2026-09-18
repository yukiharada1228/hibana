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
    pub scheme: String,
    pub authority: String,
    pub path: String,
    pub query: String,
    /// First value of each header, also readable by older Workers.
    pub headers: BTreeMap<String, String>,
    /// Remaining values in wire order. An additive field keeps rolling updates
    /// compatible with the original single-value envelope.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_headers: BTreeMap<String, Vec<String>>,
    /// Complete ordered values, base64url encoded, for names with non-ASCII
    /// bytes. These replace that name's legacy string values when reconstructed.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub encoded_headers: BTreeMap<String, Vec<String>>,
    pub body: String,
    #[serde(rename = "bodyBase64")]
    pub body_base64: bool,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            scheme: "http".into(),
            authority: String::new(),
            path: "/".into(),
            query: String::new(),
            headers: BTreeMap::new(),
            additional_headers: BTreeMap::new(),
            encoded_headers: BTreeMap::new(),
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
        let mut headers = BTreeMap::new();
        let mut additional_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut encoded_headers = BTreeMap::new();
        for name in parts.headers.keys() {
            let values = parts.headers.get_all(name);
            if values.iter().any(|value| value.to_str().is_err()) {
                encoded_headers.insert(
                    name.as_str().to_owned(),
                    values
                        .iter()
                        .map(|value| crate::b64url_encode(value.as_bytes()))
                        .collect(),
                );
            }
        }
        for (name, value) in &parts.headers {
            let Ok(value) = value.to_str() else { continue };
            match headers.entry(name.as_str().to_owned()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(value.to_owned());
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    additional_headers
                        .entry(name.as_str().to_owned())
                        .or_default()
                        .push(value.to_owned());
                }
            }
        }
        Self {
            method: parts.method.as_str().into(),
            scheme: parts.uri.scheme_str().unwrap_or("http").into(),
            authority: parts
                .uri
                .authority()
                .map(|a| a.as_str())
                .or_else(|| parts.headers.get(::http::header::HOST)?.to_str().ok())
                .unwrap_or("localhost")
                .into(),
            path: parts.uri.path().into(),
            query: parts
                .uri
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default(),
            headers,
            additional_headers,
            encoded_headers,
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
    fn preserves_repeated_values_without_breaking_legacy_envelopes() {
        let (parts, ()) = ::http::Request::builder()
            .header("accept", "text/plain")
            .header("accept", "application/json")
            .header("accept", "text/html")
            .header("cookie", "session=first")
            .header("cookie", "csrf=second")
            .body(())
            .unwrap()
            .into_parts();
        let request = HttpRequest::from_parts(&parts, &[]);
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["headers"]["accept"], "text/plain");
        assert_eq!(
            request.additional_headers["accept"],
            ["application/json", "text/html"]
        );
        assert_eq!(request.additional_headers["cookie"], ["csrf=second"]);
        assert_eq!(
            serde_json::from_value::<HttpRequest>(wire).unwrap(),
            request
        );
        let legacy: HttpRequest =
            serde_json::from_str(r#"{"headers":{"accept":"text/plain"}}"#).unwrap();
        assert!(legacy.additional_headers.is_empty());
        assert!(legacy.encoded_headers.is_empty());
        assert!(serde_json::to_value(&request)
            .unwrap()
            .get("encoded_headers")
            .is_none());
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("additional_headers")
            .is_none());
    }
    #[test]
    fn encodes_all_values_of_non_ascii_headers_without_losing_their_order() {
        let values: &[&[u8]] = &[b"first", b"caf\xe9", b"", b"\x80\xff", b"last"];
        let mut source = ::http::Request::builder().header("accept", "text/plain");
        for value in values {
            source = source.header("x-tag", *value);
        }
        let (parts, ()) = source.body(()).unwrap().into_parts();
        let input = HttpRequest::from_parts(&parts, &[]);
        let decoded: HttpRequest =
            serde_json::from_value(serde_json::to_value(input).unwrap()).unwrap();
        assert_eq!(decoded.encoded_headers.len(), 1);
        let bytes: Vec<_> = decoded.encoded_headers["x-tag"]
            .iter()
            .map(|value| crate::b64url_decode(value).unwrap())
            .collect();
        assert_eq!(bytes, values);
        assert_eq!(decoded.headers["accept"], "text/plain");
        assert_eq!(decoded.headers["x-tag"], "first");
        assert_eq!(decoded.additional_headers["x-tag"], ["", "last"]);
    }
    #[test]
    fn captures_origin_for_local_and_absolute_requests_without_trusting_forwarded_headers() {
        for (target, expected_scheme, expected_authority) in [
            ("/echo?x=%2F", "http", "localhost:8787"),
            (
                "https://app.example:8443/echo?x=%2F",
                "https",
                "app.example:8443",
            ),
        ] {
            let (parts, ()) = ::http::Request::builder()
                .uri(target)
                .header("host", "localhost:8787")
                .header("x-forwarded-proto", "https")
                .header("x-forwarded-host", "attacker.invalid")
                .body(())
                .unwrap()
                .into_parts();
            let request = HttpRequest::from_parts(&parts, &[]);
            assert_eq!(request.scheme, expected_scheme);
            assert_eq!(request.authority, expected_authority);
        }
    }
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
