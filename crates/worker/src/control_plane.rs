//! Fixed-endpoint adapter for job redemption, Secrets and idempotent result persistence.
use crate::runtime::ExecError;
use axum::http::{HeaderValue, StatusCode};
use std::{sync::Arc, time::Duration};
pub(crate) struct ControlPlaneClient {
    http: reqwest::Client,
    control_plane_internal_url: String,
    job_env_fetch_timeout: Duration,
    metrics: Arc<crate::metrics::Metrics>,
}
impl ControlPlaneClient {
    pub(crate) async fn redeem_artifact(
        &self,
        token: &HeaderValue,
    ) -> Result<hibana_shared::preparation::Artifact, StatusCode> {
        let response = self
            .http
            .post(format!(
                "{}/internal/artifact",
                self.control_plane_internal_url.trim_end_matches('/')
            ))
            .header(hibana_shared::preparation::TOKEN_HEADER, token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if !response.status().is_success() {
            return Err(if response.status().is_client_error() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            });
        }
        response.json().await.map_err(|_| StatusCode::BAD_GATEWAY)
    }
    pub(crate) fn new(
        http: reqwest::Client,
        url: String,
        timeout: Duration,
        metrics: Arc<crate::metrics::Metrics>,
    ) -> Self {
        Self {
            http,
            control_plane_internal_url: url,
            job_env_fetch_timeout: timeout,
            metrics,
        }
    }
    pub(crate) async fn redeem(
        &self,
        token: &HeaderValue,
    ) -> Result<hibana_shared::JobMessage, StatusCode> {
        let response = self
            .http
            .post(format!(
                "{}/internal/direct-job",
                self.control_plane_internal_url.trim_end_matches('/')
            ))
            .header("x-hibana-job-token", token)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let response = match response {
            Ok(r) if r.status().is_success() => r,
            Ok(r) if r.status() == StatusCode::UNAUTHORIZED => {
                return Err(StatusCode::UNAUTHORIZED)
            }
            _ => return Err(StatusCode::SERVICE_UNAVAILABLE),
        };
        response.json().await.map_err(|_| StatusCode::BAD_GATEWAY)
    }
    pub(crate) async fn complete(
        &self,
        result: &hibana_shared::ResultMessage,
    ) -> anyhow::Result<()> {
        for attempt in 0..3 {
            let response = self
                .http
                .post(format!(
                    "{}/internal/direct-result",
                    self.control_plane_internal_url.trim_end_matches('/')
                ))
                .header("x-hibana-job-token", &result.job_token)
                .json(result)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
            match response {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) if r.status().is_client_error() => anyhow::bail!("HTTP result rejected"),
                _ => {}
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        anyhow::bail!("HTTP result persistence unavailable")
    }

    pub(crate) async fn fetch_job_env(
        &self,
        env_token: &str,
    ) -> std::result::Result<
        std::collections::BTreeMap<String, hibana_shared::Redacted<String>>,
        ExecError,
    > {
        let started = std::time::Instant::now();
        let result = self.fetch_job_env_inner(env_token).await;
        let outcome = result.as_ref().err().copied().unwrap_or("ok");
        self.metrics
            .control_plane_request_duration_seconds
            .with_label_values(&["job_env", outcome])
            .observe(started.elapsed().as_secs_f64());
        result.map_err(|reason| {
            // Stable categories only: never format reqwest errors, URLs, tokens or bodies.
            tracing::warn!(
                operation = "job_env",
                reason,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Control-plane request failed before guest execution"
            );
            ExecError::Failed(format!("secret material unavailable: {reason}"))
        })
    }

    async fn fetch_job_env_inner(
        &self,
        env_token: &str,
    ) -> Result<std::collections::BTreeMap<String, hibana_shared::Redacted<String>>, &'static str>
    {
        let url = format!(
            "{}/internal/job-env",
            self.control_plane_internal_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .timeout(self.job_env_fetch_timeout)
            .json(&serde_json::json!({ "env_token": env_token }))
            .send()
            .await
            .map_err(|error| request_failure(&error))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => "authorization",
                429 => "rate_limited",
                500..=599 => "server_error",
                _ => "unexpected_status",
            });
        }

        #[derive(serde::Deserialize)]
        struct JobEnvResponse {
            env: std::collections::BTreeMap<String, String>,
        }
        let body: JobEnvResponse = resp.json().await.map_err(|error| request_failure(&error))?;

        Ok(body
            .env
            .into_iter()
            .map(|(k, v)| (k, hibana_shared::Redacted::new(v)))
            .collect())
    }
}

fn request_failure(error: &reqwest::Error) -> &'static str {
    // A response-body timeout can also be marked as a decode error.
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_decode() {
        "malformed_response"
    } else {
        "transport"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, response::Response, routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn secret_failures_are_classified_without_retries_or_sensitive_details() {
        for (case, expected) in [
            ("ok", "ok"),
            ("headers", "timeout"),
            ("body", "timeout"),
            ("invalid", "malformed_response"),
            ("denied", "authorization"),
            ("rate", "rate_limited"),
            ("server", "server_error"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/sensitive-path", listener.local_addr().unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let app = Router::new().route(
                "/sensitive-path/internal/job-env",
                post(move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if case == "headers" {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                        if case == "body" {
                            let body = Body::from_stream(futures::stream::once(async {
                                tokio::time::sleep(Duration::from_secs(10)).await;
                                Ok::<_, std::io::Error>("secret-response-body")
                            }));
                            return Response::new(body);
                        }
                        let (status, body) = match case {
                            "ok" => (200, r#"{"env":{"TOKEN":"secret-response-body"}}"#),
                            "denied" => (403, "secret-response-body"),
                            "rate" => (429, "secret-response-body"),
                            "server" => (503, "secret-response-body"),
                            _ => (200, "invalid-secret-response-body"),
                        };
                        Response::builder()
                            .status(status)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let metrics = crate::metrics::Metrics::init();
            let client = ControlPlaneClient::new(
                reqwest::Client::builder().no_proxy().build().unwrap(),
                endpoint,
                Duration::from_millis(200),
                metrics.clone(),
            );
            let result = client.fetch_job_env("secret-request-token").await;
            server.abort();
            if expected == "ok" {
                let Ok(env) = result else {
                    panic!("Secret request failed")
                };
                assert_eq!(env["TOKEN"].expose(), "secret-response-body");
            } else {
                let Err(ExecError::Failed(error)) = result else {
                    panic!("Expected Secret request failure")
                };
                assert!(error.ends_with(expected), "{case}: {error}");
                assert!(
                    !error.contains("sensitive-path") && !error.contains("secret-response-body")
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "no automatic retries");
            let (_, rendered) = metrics.render();
            assert!(rendered.contains(&format!("operation=\"job_env\",outcome=\"{expected}\"")));
            assert!(
                !rendered.contains("secret-request-token")
                    && !rendered.contains("secret-response-body")
            );
        }
    }

    #[tokio::test]
    async fn secret_connection_failure_has_a_distinct_category() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = ControlPlaneClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            endpoint,
            Duration::from_secs(2),
            crate::metrics::Metrics::init(),
        );
        assert!(matches!(client.fetch_job_env("token").await,
            Err(ExecError::Failed(reason)) if reason.ends_with("connect")));
    }
}
