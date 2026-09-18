//! Bounded response channel and request conversion. No fleet services, DB or Axum dependency.
use anyhow::anyhow;
use bytes::Bytes;
use hibana_shared::http::HttpRequest;
use http_body_util::{BodyExt, Full};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot};
#[derive(Debug)]
pub(crate) struct HttpResponseReceipt {
    pub status: u16,
}
pub struct ResponseSender {
    headers: oneshot::Sender<hyper::http::response::Parts>,
    body: mpsc::Sender<Bytes>,
    pub(super) bytes: Arc<AtomicU64>,
}

impl ResponseSender {
    pub async fn send(
        self,
        response: hyper::Response<wasmtime_wasi_http::body::HyperOutgoingBody>,
    ) -> anyhow::Result<HttpResponseReceipt> {
        let (parts, mut body) = response.into_parts();
        let status = parts.status.as_u16();
        self.headers
            .send(parts)
            .map_err(|_| anyhow::anyhow!("HTTP client disconnected"))?;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| anyhow::anyhow!("Guest response stream failed"))?;
            if let Ok(data) = frame.into_data() {
                let size =
                    self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
                anyhow::ensure!(
                    size <= hibana_shared::http::MAX_RESPONSE_BYTES as u64,
                    "HTTP response exceeds 64 MiB"
                );
                // Split frames as well as bounding their count: no giant frame queues.
                for part in data.chunks(16 * 1024) {
                    self.body
                        .send(Bytes::copy_from_slice(part))
                        .await
                        .map_err(|_| anyhow::anyhow!("HTTP client disconnected"))?;
                }
            }
        }
        // Response content is deliberately not stored in the shared execution log.
        Ok(HttpResponseReceipt { status })
    }
}

pub(crate) struct ResponseReceiver {
    headers: oneshot::Receiver<hyper::http::response::Parts>,
    body: ResponseBody,
}

struct ResponseBody {
    chunks: mpsc::Receiver<Bytes>,
    completed: Option<oneshot::Receiver<()>>,
}

impl ResponseBody {
    async fn next(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if let Some(chunk) = self.chunks.recv().await {
            return Some(Ok(chunk));
        }
        if let Some(completed) = self.completed.as_mut() {
            let result = completed.await;
            self.completed = None;
            if result.is_err() {
                return Some(Err(std::io::Error::other(
                    "Invocation did not complete successfully",
                )));
            }
        }
        None
    }
}

/// The completion sender is separate from body backpressure. Send only after
/// successful execution and durable result publication; dropping it fails closed.
pub(crate) fn response_channel() -> (ResponseSender, ResponseReceiver, oneshot::Sender<()>) {
    let (headers, head_rx) = oneshot::channel();
    let (body, body_rx) = mpsc::channel(4);
    let (completed, completion_rx) = oneshot::channel();
    (
        ResponseSender {
            headers,
            body,
            bytes: Arc::new(AtomicU64::new(0)),
        },
        ResponseReceiver {
            headers: head_rx,
            body: ResponseBody {
                chunks: body_rx,
                completed: Some(completion_rx),
            },
        },
        completed,
    )
}
impl ResponseReceiver {
    pub(crate) async fn receive(
        mut self,
        is_head: bool,
    ) -> anyhow::Result<(
        hyper::http::response::Parts,
        impl futures::Stream<Item = Result<Bytes, std::io::Error>>,
    )> {
        let mut parts = self
            .headers
            .await
            .map_err(|_| anyhow!("Worker did not produce an HTTP response"))?;
        // EOF must wait for execution/result persistence, regardless of guest framing.
        parts.headers.remove("content-length");
        parts.headers.remove("transfer-encoding");
        // HTTP transports discard bodies for these responses, including HEAD at
        // the control plane. Hold their headers until completion is confirmed
        // so execution/persistence failures cannot become successful empty replies.
        if is_head || matches!(parts.status.as_u16(), 204 | 304) || parts.status.is_informational()
        {
            while let Some(chunk) = self.body.next().await {
                chunk?;
            }
        }
        let body = futures::stream::unfold(self.body, |mut body| async {
            body.next().await.map(|item| (item, body))
        });
        Ok((parts, body))
    }
}
type ReqBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;
pub(super) fn into_request(input: HttpRequest) -> anyhow::Result<hyper::Request<ReqBody>> {
    let body = input.body_bytes().map_err(|e| anyhow!(e))?;
    anyhow::ensure!(
        matches!(input.scheme.as_str(), "http" | "https"),
        "invalid HTTP scheme"
    );
    // Older envelopes have no authority field. Preserve their Host header when
    // draining requests accepted before a rolling update.
    let authority = if input.authority.is_empty() {
        input
            .headers
            .get("host")
            .map(String::as_str)
            .unwrap_or("localhost")
    } else {
        input.authority.as_str()
    };
    let uri = hyper::Uri::builder()
        .scheme(input.scheme.as_str())
        .authority(authority)
        .path_and_query(format!("{}{}", input.path, input.query))
        .build()?;
    let mut builder = hyper::Request::builder()
        .method(input.method.as_str())
        .uri(uri);
    for (name, value) in input.headers {
        builder = builder.header(name, value);
    }
    for (name, values) in input.additional_headers {
        for value in values {
            builder = builder.header(&name, value);
        }
    }
    let mut request = builder
        .body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed())
        .map_err(|e| anyhow!("failed to build request: {e}"))?;
    for (name, values) in input.encoded_headers {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())?;
        anyhow::ensure!(!values.is_empty(), "empty encoded HTTP header list");
        // Replace all legacy values for this name, including case variants.
        // Encoding only the non-ASCII values would lose their interleaving.
        request.headers_mut().remove(&name);
        for value in values {
            let bytes = hibana_shared::b64url_decode(&value)
                .ok_or_else(|| anyhow!("invalid base64 HTTP header"))?;
            request
                .headers_mut()
                .append(&name, hyper::header::HeaderValue::from_bytes(&bytes)?);
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    #[test]
    fn reconstructs_repeated_headers_in_order_and_accepts_old_envelopes() {
        for wire in [
            r#"{"headers":{"host":"app.example","accept":"text/plain","cookie":"session=first"},"additional_headers":{"accept":["application/json","text/html"],"cookie":["csrf=second"]}}"#,
            r#"{"headers":{"host":"app.example","accept":"text/plain","cookie":"session=first"}}"#,
        ] {
            let envelope: HttpRequest = serde_json::from_str(wire).unwrap();
            let multiple = !envelope.additional_headers.is_empty();
            let request = into_request(envelope).unwrap();
            let values = |name| {
                request
                    .headers()
                    .get_all(name)
                    .iter()
                    .map(|v| v.to_str().unwrap())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                values("accept"),
                if multiple {
                    vec!["text/plain", "application/json", "text/html"]
                } else {
                    vec!["text/plain"]
                }
            );
            assert_eq!(
                values("cookie"),
                if multiple {
                    vec!["session=first", "csrf=second"]
                } else {
                    vec!["session=first"]
                }
            );
            assert_eq!(request.uri().authority().unwrap(), "app.example");
        }
    }

    #[test]
    fn preserves_raw_header_bytes_and_interleaved_repeated_values() {
        let values: &[&[u8]] = &[b"first", b"caf\xe9", b"", b"\x80\xff", b"last"];
        let mut source = hyper::Request::builder().header("accept", "text/plain");
        for value in values {
            source = source.header("x-tag", *value);
        }
        let (parts, ()) = source.body(()).unwrap().into_parts();
        let envelope = HttpRequest::from_parts(&parts, &[]);
        let wire = serde_json::to_vec(&envelope).unwrap();
        let request = into_request(serde_json::from_slice(&wire).unwrap()).unwrap();
        let actual: Vec<_> = request
            .headers()
            .get_all("x-tag")
            .iter()
            .map(|value| value.as_bytes())
            .collect();
        assert_eq!(
            actual, values,
            "encoded values replace legacy values, without duplicating them"
        );
        assert_eq!(request.headers()["accept"], "text/plain");
    }

    #[test]
    fn rejects_invalid_encoded_headers_before_execution() {
        for (name, values) in [
            ("bad name", vec![String::new()]),
            ("x-tag", vec![]),
            ("x-tag", vec!["%invalid".into()]),
            ("x-tag", vec![hibana_shared::b64url_encode(b"x\r\ny")]),
            ("x-tag", vec![hibana_shared::b64url_encode(b"x\0y")]),
        ] {
            let mut envelope = HttpRequest::default();
            envelope.encoded_headers.insert(name.into(), values);
            assert!(into_request(envelope).is_err());
        }
    }

    #[test]
    fn preserves_origin_port_and_raw_path_without_resolving_against_a_base_url() {
        let mut request = HttpRequest {
            scheme: "https".into(),
            authority: "hello.team.example:8443".into(),
            path: "//other/path%2Fsegment".into(),
            query: "?next=%2F&x=1".into(),
            ..Default::default()
        };
        assert_eq!(
            into_request(request.clone()).unwrap().uri().to_string(),
            "https://hello.team.example:8443//other/path%2Fsegment?next=%2F&x=1"
        );
        request.authority.clear();
        request
            .headers
            .insert("host".into(), "localhost:8787".into());
        assert_eq!(
            into_request(request.clone())
                .unwrap()
                .uri()
                .authority()
                .unwrap(),
            "localhost:8787"
        );
        request.scheme = "file".into();
        assert!(into_request(request).is_err());
    }

    #[tokio::test]
    async fn streams_bounded_chunks_and_keeps_eof_behind_completion() {
        let payload = Bytes::from(vec![0xa5; 64 * 1024 + 1]);
        let (sender, receiver, completion) = response_channel();
        let bytes = sender.bytes.clone();
        let response = hyper::Response::builder()
            .status(201)
            .header("content-length", payload.len())
            .header("transfer-encoding", "chunked")
            .header("content-type", "application/octet-stream")
            .body(Full::new(payload.clone()).map_err(|e| match e {}).boxed())
            .unwrap();
        let task = tokio::spawn(sender.send(response));
        let (parts, body) = receiver.receive(false).await.unwrap();
        assert_eq!(parts.status, 201);
        assert_eq!(parts.headers["content-type"], "application/octet-stream");
        assert!(!parts.headers.contains_key("content-length"));
        assert!(!parts.headers.contains_key("transfer-encoding"));
        futures::pin_mut!(body);
        let mut received = Vec::new();
        while received.len() < payload.len() {
            let chunk = body.next().await.unwrap().unwrap();
            assert!(chunk.len() <= 16 * 1024);
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, payload);
        assert_eq!(task.await.unwrap().unwrap().status, 201);
        assert_eq!(bytes.load(Ordering::Relaxed), payload.len() as u64);
        assert!(tokio::time::timeout(Duration::from_millis(20), body.next())
            .await
            .is_err());
        completion.send(()).unwrap();
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn persistence_failure_reaches_client_after_headers() {
        let (sender, receiver, completion) = response_channel();
        sender
            .send(hyper::Response::new(
                Full::new(Bytes::new()).map_err(|e| match e {}).boxed(),
            ))
            .await
            .unwrap();
        let (_, body) = receiver.receive(false).await.unwrap();
        futures::pin_mut!(body);
        drop(completion);
        assert!(body.next().await.unwrap().is_err());
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn bodyless_responses_wait_for_completion_and_propagate_failures() {
        for (is_head, status) in [(true, 200), (true, 404), (false, 204), (false, 304)] {
            for fails in [false, true] {
                let (sender, receiver, completion) = response_channel();
                // Drain even a guest that writes a body for HEAD; do not deadlock
                // on the bounded queue before the execution can complete.
                let send = tokio::spawn(
                    sender.send(
                        hyper::Response::builder()
                            .status(status)
                            .body(
                                Full::new(Bytes::from(vec![0; 128 * 1024]))
                                    .map_err(|e| match e {})
                                    .boxed(),
                            )
                            .unwrap(),
                    ),
                );
                let receive = receiver.receive(is_head);
                tokio::pin!(receive);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut receive)
                        .await
                        .is_err()
                );
                send.await.unwrap().unwrap();
                if fails {
                    drop(completion);
                } else {
                    completion.send(()).unwrap();
                }
                let result = receive.await;
                if fails {
                    assert!(result.is_err(), "{is_head} {status}");
                } else {
                    let (parts, body) = result.unwrap();
                    assert_eq!(parts.status, status);
                    futures::pin_mut!(body);
                    assert!(body.next().await.is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn client_disconnect_unblocks_a_backpressured_sender() {
        let (sender, receiver, _completion) = response_channel();
        let task = tokio::spawn(
            sender.send(hyper::Response::new(
                Full::new(Bytes::from(vec![0; 128 * 1024]))
                    .map_err(|e| match e {})
                    .boxed(),
            )),
        );
        let (_, body) = receiver.receive(false).await.unwrap();
        // More than four chunks cannot be queued without a consumer.
        drop(body);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("disconnected clients must release the execution")
            .unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn timeout_releases_execution_while_the_client_keeps_the_body_queue_full() {
        let (sender, receiver, completion) = response_channel();
        let task = tokio::spawn(async move {
            let response = hyper::Response::new(
                Full::new(Bytes::from(vec![0; 128 * 1024]))
                    .map_err(|e| match e {})
                    .boxed(),
            );
            let result =
                tokio::time::timeout(Duration::from_millis(20), sender.send(response)).await;
            assert!(
                result.is_err(),
                "the client must keep the producer backpressured"
            );
            drop(completion);
        });
        let (_, body) = receiver.receive(false).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("execution completion must not wait for the client")
            .unwrap();
        futures::pin_mut!(body);
        let mut received = 0;
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => received += bytes.len(),
                Err(_) => {
                    assert_eq!(received, 4 * 16 * 1024);
                    assert!(body.next().await.is_none());
                    return;
                }
            }
        }
        panic!("a timed out execution must fail the stream instead of returning normal EOF");
    }
}
