//! Discover ready Workers through DNS; retry only an explicit pre-execution refusal.
use hibana_shared::http::WORKER_REJECTED_HEADER;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

static NEXT_WORKER: AtomicUsize = AtomicUsize::new(0);
const MAX_ATTEMPTS: usize = 8;

pub(crate) async fn send(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
) -> anyhow::Result<reqwest::Response> {
    let started = std::time::Instant::now();
    let mut targets = discover(endpoint, "invoke").await.inspect_err(|_| {
        tracing::warn!(
            stage = "discovery",
            reason = "worker_discovery_failed",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Worker dispatch failed"
        );
    })?;
    tracing::debug!(target: "hibana_latency", stage = "worker_discovery",
        elapsed_us = started.elapsed().as_micros() as u64, "HTTP phase timing");
    let start = NEXT_WORKER.fetch_add(1, Ordering::Relaxed) % targets.len();
    targets.rotate_left(start);
    send_targets(client, &targets, token).await
}

pub(crate) async fn discover(endpoint: &str, operation: &str) -> anyhow::Result<Vec<reqwest::Url>> {
    let mut url = reqwest::Url::parse(endpoint)?;
    url.set_path(&format!("{}/{operation}", url.path().trim_end_matches('/')));
    let mut targets = vec![url.clone()];
    // A headless Service provides ready Pod IPs without Kubernetes API credentials.
    // HTTPS keeps the configured hostname for TLS validation (use an upstream LB).
    if url.scheme() == "http" {
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Worker host missing"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("Worker port missing"))?;
        let addresses = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::lookup_host((host, port)),
        )
        .await??;
        targets.clear();
        for address in addresses {
            let mut target = url.clone();
            target
                .set_ip_host(address.ip())
                .map_err(|_| anyhow::anyhow!("Invalid Worker IP"))?;
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    }
    if targets.is_empty() {
        anyhow::bail!("No ready Workers");
    }
    Ok(targets)
}

async fn send_targets(
    client: &reqwest::Client,
    targets: &[reqwest::Url],
    token: &str,
) -> anyhow::Result<reqwest::Response> {
    let count = targets.len().min(MAX_ATTEMPTS);
    for (index, target) in targets.iter().take(count).enumerate() {
        let started = std::time::Instant::now();
        // Transport errors are ambiguous: never replay them, even on another Pod.
        let response = client
            .post(target.clone())
            .header("x-hibana-job-token", token)
            .send()
            .await
            .inspect_err(|error| {
                let reason = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else {
                    "transport"
                };
                tracing::warn!(
                    stage = "send",
                    reason,
                    attempt = index + 1,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Worker dispatch failed; invocation will not be replayed"
                );
            })?;
        if !retryable_refusal(&response) || index + 1 == count {
            return Ok(response);
        }
    }
    anyhow::bail!("No Worker targets")
}

pub(crate) fn retryable_refusal(response: &reqwest::Response) -> bool {
    response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && response
            .headers()
            .get(WORKER_REJECTED_HEADER)
            .is_some_and(|v| v == "capacity" || v == "not_prepared")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    async fn server(
        status: axum::http::StatusCode,
        marker: &'static str,
    ) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/invoke", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let app = Router::new().route(
            "/invoke",
            post(move || async move {
                let mut headers = axum::http::HeaderMap::new();
                if !marker.is_empty() {
                    headers.insert(WORKER_REJECTED_HEADER, marker.parse().unwrap());
                }
                (status, headers, "response")
            }),
        );
        (
            url,
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        )
    }
    #[tokio::test]
    async fn only_explicit_capacity_refusal_can_try_another_worker() {
        use axum::http::StatusCode;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (busy, a) = server(StatusCode::SERVICE_UNAVAILABLE, "capacity").await;
        let (guest_error, b) = server(StatusCode::SERVICE_UNAVAILABLE, "").await;
        let (healthy, c) = server(StatusCode::OK, "").await;
        let (cold, d) = server(StatusCode::SERVICE_UNAVAILABLE, "not_prepared").await;
        assert_eq!(
            send_targets(&client, &[cold, healthy.clone()], "token")
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send_targets(&client, &[busy, healthy.clone()], "token")
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send_targets(&client, &[guest_error, healthy.clone()], "token")
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}/invoke", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        drop(listener);
        assert!(send_targets(&client, &[dead, healthy], "token")
            .await
            .is_err());
        a.abort();
        b.abort();
        c.abort();
        d.abort();
    }
}
