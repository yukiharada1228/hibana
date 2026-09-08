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
    pub(crate) guest_memory_budget_mib: u32,
    pub(crate) max_compilations: usize,
    pub(crate) compiler_limits: crate::compiler::Limits,
    pub(crate) db_max_connections: u32,
    pub(crate) drain_timeout_secs: u64,
}

const DEFAULT_WASM_CACHE_DIR: &str = "./worker-cache";
const DEFAULT_METRICS_BIND_ADDR: &str = "0.0.0.0:9090";
const DEFAULT_CONTROL_PLANE_INTERNAL_URL: &str = "http://127.0.0.1:8081";
const DEFAULT_JOB_ENV_FETCH_TIMEOUT_MS: u64 = 2000;
const DEFAULT_WORKER_MAX_CONCURRENCY: u64 = 8;
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
            max_concurrency: bounded(
                "WORKER_MAX_CONCURRENCY",
                DEFAULT_WORKER_MAX_CONCURRENCY,
                1,
                1024,
            )?,
            guest_memory_budget_mib: bounded("WORKER_GUEST_MEMORY_BUDGET_MIB", 2048, 1, 1_048_576)?
                as u32,
            max_compilations: bounded("WORKER_MAX_COMPILATIONS", 1, 1, 8)? as usize,
            compiler_limits: crate::compiler::Limits {
                memory_mib: bounded("WORKER_COMPILER_MEMORY_MIB", 1024, 256, 4096)?,
                timeout_secs: bounded("WORKER_COMPILER_TIMEOUT_SECS", 60, 1, 300)?,
            },
            db_max_connections: bounded("WORKER_DB_MAX_CONNECTIONS", 8, 1, 128)? as u32,
            drain_timeout_secs: env_u64(
                "WORKER_DRAIN_TIMEOUT_SECS",
                DEFAULT_WORKER_DRAIN_TIMEOUT_SECS,
            )?,
        })
    }
}

fn bounded(key: &str, default: u64, min: u64, max: u64) -> anyhow::Result<u64> {
    let value = env_u64(key, default)?;
    anyhow::ensure!(
        (min..=max).contains(&value),
        "{key} must be between {min} and {max}"
    );
    Ok(value)
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
