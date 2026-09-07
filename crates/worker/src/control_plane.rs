//! Fixed-endpoint adapter for job redemption, Secrets and idempotent result persistence.
use crate::runtime::ExecError;
use axum::http::{HeaderValue, StatusCode};
use std::time::Duration;
pub(crate) struct ControlPlaneClient {
    http: reqwest::Client,
    control_plane_internal_url: String,
    job_env_fetch_timeout: Duration,
}
impl ControlPlaneClient {
    pub(crate) fn new(http: reqwest::Client, url: String, timeout: Duration) -> Self {
        Self {
            http,
            control_plane_internal_url: url,
            job_env_fetch_timeout: timeout,
        }
    }
    pub(crate) async fn redeem(
        &self,
        token: &HeaderValue,
    ) -> Result<faas_shared::JobMessage, StatusCode> {
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
    pub(crate) async fn complete(&self, result: &faas_shared::ResultMessage) -> anyhow::Result<()> {
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
        std::collections::BTreeMap<String, faas_shared::Redacted<String>>,
        ExecError,
    > {
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
            .map_err(|e| {
                // reqwest のエラー表示には URL しか出ない（body は含まれない）。
                ExecError::Failed(format!("secret material unavailable: {}", e.without_url()))
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ExecError::Failed(format!(
                "secret material unavailable: control-plane returned {status}"
            )));
        }

        #[derive(serde::Deserialize)]
        struct JobEnvResponse {
            env: std::collections::BTreeMap<String, String>,
        }
        let body: JobEnvResponse = resp.json().await.map_err(|_| {
            ExecError::Failed("secret material unavailable: malformed response".into())
        })?;

        Ok(body
            .env
            .into_iter()
            .map(|(k, v)| (k, faas_shared::Redacted::new(v)))
            .collect())
    }
}
