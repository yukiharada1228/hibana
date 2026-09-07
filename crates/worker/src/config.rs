//! Environment configuration for the production Worker.
use anyhow::Context as _;
use std::path::PathBuf;
pub(crate) struct Settings {
    pub(crate) database_url: String,
    pub(crate) wasm_cache_dir: PathBuf,
    pub(crate) metrics_bind_addr: String,
    pub(crate) http_bind_addr: String,
    pub(crate) control_plane_internal_url: String,
    pub(crate) job_env_fetch_timeout_ms: u64,
    pub(crate) max_concurrency: u64,
    pub(crate) drain_timeout_secs: u64,
}

const DEFAULT_WASM_CACHE_DIR: &str = "./worker-cache";
const DEFAULT_METRICS_BIND_ADDR: &str = "0.0.0.0:9090";
const DEFAULT_CONTROL_PLANE_INTERNAL_URL: &str = "http://127.0.0.1:8081";
const DEFAULT_JOB_ENV_FETCH_TIMEOUT_MS: u64 = 2000;
const DEFAULT_WORKER_MAX_CONCURRENCY: u64 = 32;
const DEFAULT_WORKER_DRAIN_TIMEOUT_SECS: u64 = 30;
impl Settings {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        let wasm_cache_dir = std::env::var("WASM_CACHE_DIR")
            .unwrap_or_else(|_| DEFAULT_WASM_CACHE_DIR.to_string())
            .into();
        let metrics_bind_addr = std::env::var("METRICS_BIND_ADDR")
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|_| DEFAULT_METRICS_BIND_ADDR.to_string());
        Ok(Self {
            database_url,
            wasm_cache_dir,
            metrics_bind_addr,
            http_bind_addr: std::env::var("WORKER_HTTP_BIND_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8082".into()),
            control_plane_internal_url: std::env::var("CONTROL_PLANE_INTERNAL_URL")
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|_| DEFAULT_CONTROL_PLANE_INTERNAL_URL.to_string()),
            job_env_fetch_timeout_ms: env_u64(
                "JOB_ENV_FETCH_TIMEOUT_MS",
                DEFAULT_JOB_ENV_FETCH_TIMEOUT_MS,
            )?,
            max_concurrency: env_u64("WORKER_MAX_CONCURRENCY", DEFAULT_WORKER_MAX_CONCURRENCY)?
                .max(1),
            drain_timeout_secs: env_u64(
                "WORKER_DRAIN_TIMEOUT_SECS",
                DEFAULT_WORKER_DRAIN_TIMEOUT_SECS,
            )?,
        })
    }
}

fn env_u64(key: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("env var {key} must be a non-negative integer")),
        Err(_) => Ok(default),
    }
}
