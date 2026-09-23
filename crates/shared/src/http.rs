//! Language-neutral HTTP request stored by the Control Plane and consumed by the runtime.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Internal transport marker. Guest responses must never set this header.
pub const WORKER_REJECTED_HEADER: &str = "x-hibana-worker-rejected";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpRequest {
    pub method: String,
    pub scheme: String,
    pub authority: String,
    pub path: String,
    pub query: String,
    /// Complete ordered values for each header, base64url encoded to preserve
    /// raw bytes through JSONB. Names come from http::HeaderMap and are lowercase.
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: String,
    #[serde(rename = "bodyBase64")]
    pub body_base64: bool,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            scheme: "http".into(),
            authority: "localhost".into(),
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
            // PostgreSQL JSONB cannot store NUL, even in valid UTF-8 text.
            Ok(text) if !text.contains('\0') => (text.to_owned(), false),
            _ => (crate::b64url_encode(body), true),
        };
        let headers = parts
            .headers
            .keys()
            .map(|name| {
                (
                    name.as_str().to_owned(),
                    parts
                        .headers
                        .get_all(name)
                        .iter()
                        .map(|value| crate::b64url_encode(value.as_bytes()))
                        .collect(),
                )
            })
            .collect();
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
            body,
            body_base64,
        }
    }

    /// Reuse the text buffer when consuming an envelope; encoded bodies need decoding.
    pub fn into_body_bytes(self) -> Result<Vec<u8>, &'static str> {
        if self.body_base64 {
            crate::b64url_decode(&self.body).ok_or("invalid base64 HTTP body")
        } else {
            Ok(self.body.into_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_repeated_values_once_in_wire_order() {
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
        let decoded: HttpRequest = serde_json::from_value(wire.clone()).unwrap();
        let values = |name: &str| {
            decoded.headers[name]
                .iter()
                .map(|value| crate::b64url_decode(value).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values("accept"),
            [b"text/plain".as_slice(), b"application/json", b"text/html"]
        );
        assert_eq!(
            values("cookie"),
            [b"session=first".as_slice(), b"csrf=second"]
        );
        assert_eq!(decoded, request);
        assert!(wire.get("additional_headers").is_none());
        assert!(wire.get("encoded_headers").is_none());
    }

    #[test]
    fn preserves_raw_header_bytes_without_an_ascii_copy() {
        let values: &[&[u8]] = &[b"first", b"caf\xe9", b"", b"\x80\xff", b"\t", b"last"];
        let mut source = ::http::Request::builder();
        for value in values {
            source = source.header("x-tag", *value);
        }
        let (parts, ()) = source.body(()).unwrap().into_parts();
        let input = HttpRequest::from_parts(&parts, &[]);
        let decoded: HttpRequest =
            serde_json::from_value(serde_json::to_value(input).unwrap()).unwrap();
        assert_eq!(decoded.headers.len(), 1);
        let bytes: Vec<_> = decoded.headers["x-tag"]
            .iter()
            .map(|value| crate::b64url_decode(value).unwrap())
            .collect();
        assert_eq!(bytes, values);
    }

    #[test]
    fn rejects_obsolete_header_formats_instead_of_dropping_values() {
        for wire in [
            r#"{"headers":{"accept":"text/plain"}}"#,
            r#"{"additional_headers":{"accept":["text/html"]}}"#,
            r#"{"encoded_headers":{"x-tag":["YWJj"]}}"#,
        ] {
            assert!(serde_json::from_str::<HttpRequest>(wire).is_err());
        }
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
        for (body, encoded) in [
            (b"hello".as_slice(), false),
            (b"line\n\tend", false),
            ("雪".as_bytes(), false),
            (b"", false),
            (b"\0", true),
            (b"hello\0world", true),
            (b"\0hello", true),
            (b"hello\0", true),
            ("雪\0".as_bytes(), true),
            (&[0, 255, 128, 10], true),
            (&[255, 128], true),
        ] {
            let (parts, ()) = ::http::Request::builder()
                .method("POST")
                .uri("/echo?a=%2F")
                .header("content-type", "application/octet-stream")
                .body(())
                .unwrap()
                .into_parts();
            let input = HttpRequest::from_parts(&parts, body);
            assert_eq!(input.body_base64, encoded, "body: {body:?}");
            let decoded: HttpRequest =
                serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
            assert_eq!(decoded.method, "POST");
            assert_eq!(decoded.path, "/echo");
            assert_eq!(decoded.query, "?a=%2F");
            assert_eq!(
                crate::b64url_decode(&decoded.headers["content-type"][0]).unwrap(),
                b"application/octet-stream"
            );
            assert_eq!(decoded.into_body_bytes().unwrap(), body);
        }
    }
    #[test]
    fn missing_body_is_empty_and_corrupt_encoded_body_is_rejected() {
        let request: HttpRequest =
            serde_json::from_str(r#"{"method":"GET","path":"/empty","headers":{}}"#).unwrap();
        assert!(request.into_body_bytes().unwrap().is_empty());
        let corrupt: HttpRequest =
            serde_json::from_str(r#"{"body":"%invalid","bodyBase64":true}"#).unwrap();
        assert!(corrupt.into_body_bytes().is_err());
    }
}
