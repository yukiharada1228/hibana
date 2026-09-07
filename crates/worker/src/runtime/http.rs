//! Bounded response channel and request conversion. No fleet services, DB or Axum dependency.
use anyhow::anyhow;
use bytes::Bytes;
use faas_shared::http::HttpRequest;
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
    pub(crate) body: mpsc::Sender<Result<Bytes, std::io::Error>>,
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
                    size <= faas_shared::http::MAX_RESPONSE_BYTES as u64,
                    "HTTP response exceeds 64 MiB"
                );
                // Split frames as well as bounding their count: no giant frame queues.
                for part in data.chunks(16 * 1024) {
                    self.body
                        .send(Ok(Bytes::copy_from_slice(part)))
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
    body: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}
pub(crate) fn response_channel() -> (ResponseSender, ResponseReceiver) {
    let (headers, head_rx) = oneshot::channel();
    let (body, body_rx) = mpsc::channel(4);
    (
        ResponseSender {
            headers,
            body,
            bytes: Arc::new(AtomicU64::new(0)),
        },
        ResponseReceiver {
            headers: head_rx,
            body: body_rx,
        },
    )
}
impl ResponseReceiver {
    pub(crate) async fn receive(
        self,
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
        let body = futures::stream::unfold(self.body, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok((parts, body))
    }
}
type ReqBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;
pub(super) fn into_request(input: HttpRequest) -> anyhow::Result<hyper::Request<ReqBody>> {
    let body = input.body_bytes().map_err(|e| anyhow!(e))?;
    let uri = format!("http://hibana.local{}{}", input.path, input.query);
    let mut builder = hyper::Request::builder()
        .method(input.method.as_str())
        .uri(uri);
    for (name, value) in input.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed())
        .map_err(|e| anyhow!("failed to build request: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    #[tokio::test]
    async fn streams_bounded_chunks_and_keeps_eof_behind_completion() {
        let payload = Bytes::from(vec![0xa5; 64 * 1024 + 1]);
        let (sender, receiver) = response_channel();
        // The service holds this until the handler and result persistence finish.
        let completion = sender.body.clone();
        let bytes = sender.bytes.clone();
        let response = hyper::Response::builder()
            .status(201)
            .header("content-length", payload.len())
            .header("transfer-encoding", "chunked")
            .header("content-type", "application/octet-stream")
            .body(Full::new(payload.clone()).map_err(|e| match e {}).boxed())
            .unwrap();
        let task = tokio::spawn(sender.send(response));
        let (parts, body) = receiver.receive().await.unwrap();
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
        drop(completion);
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn persistence_failure_reaches_client_after_headers() {
        let (sender, receiver) = response_channel();
        let completion = sender.body.clone();
        sender
            .send(hyper::Response::new(
                Full::new(Bytes::new()).map_err(|e| match e {}).boxed(),
            ))
            .await
            .unwrap();
        let (_, body) = receiver.receive().await.unwrap();
        futures::pin_mut!(body);
        completion
            .send(Err(std::io::Error::other("persistence failed")))
            .await
            .unwrap();
        drop(completion);
        assert!(body.next().await.unwrap().is_err());
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn client_disconnect_unblocks_a_backpressured_sender() {
        let (sender, receiver) = response_channel();
        let task = tokio::spawn(
            sender.send(hyper::Response::new(
                Full::new(Bytes::from(vec![0; 128 * 1024]))
                    .map_err(|e| match e {})
                    .boxed(),
            )),
        );
        let (_, body) = receiver.receive().await.unwrap();
        // More than four chunks cannot be queued without a consumer.
        drop(body);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("disconnected clients must release the execution")
            .unwrap();
        assert!(result.is_err());
    }
}
