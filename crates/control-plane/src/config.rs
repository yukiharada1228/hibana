use anyhow::Context;
use hibana_shared::Redacted;

const DEFAULT_MAX_WASM_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_PRESIGN_TTL_SECS: u64 = 300;

const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6379";
const DEFAULT_INVOKE_RATE_PER_SEC: u64 = 50;
const DEFAULT_INVOKE_BURST: u64 = 500;
const DEFAULT_MAX_CONCURRENT: u64 = 20;
const DEFAULT_INFLIGHT_TTL_SECS: u64 = 3600;
const DEFAULT_LOGIN_LOCKOUT_THRESHOLD: u64 = 10;
const DEFAULT_LOGIN_LOCKOUT_WINDOW_SECS: u64 = 900;
const DEFAULT_REAPER_INTERVAL_SECS: u64 = 30;
const DEFAULT_STUCK_EXECUTION_DEADLINE_SECS: u64 = 900;

const DEFAULT_INTERNAL_BIND_ADDR: &str = "127.0.0.1:8081";
// An admitted request may redeem Secrets once before executing. The default
// internal budget must cover the public default rate plus its initial burst.
// Operators raising tenant quotas or sharing many tenants across Worker IPs must
// also size this per-tenant/per-peer protection explicitly.
const DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN: u64 =
    DEFAULT_INVOKE_RATE_PER_SEC * 60 + DEFAULT_INVOKE_BURST;

const DEFAULT_METRICS_INCLUDE_TENANT_LABEL: bool = true;

const SECRETS_MASTER_KEY_PLACEHOLDER: &str = "CHANGE_ME_REPLACE_WITH_32_BYTE_KEY_BEFORE_USE";

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub migration_database_url: String,
    pub bootstrap_admin_token: Redacted<String>,
    pub bind_addr: String,

    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_access_key: String,
    pub s3_secret_key: Redacted<String>,

    pub max_wasm_upload_bytes: u64,
    pub presign_ttl_secs: u64,

    pub job_signing_key: Redacted<String>,
    pub job_signing_kid: String,
    pub token_margin_secs: u64,

    pub redis_url: String,
    pub invoke_rate_per_sec: u64,
    pub invoke_burst: u64,
    pub max_concurrent_executions: u64,
    pub inflight_ttl_secs: u64,
    pub login_lockout_threshold: u64,
    pub login_lockout_window_secs: u64,
    pub reaper_interval_secs: u64,
    pub stuck_execution_deadline_secs: u64,
    pub trust_proxy_headers: bool,

    pub secrets_master_key: Redacted<String>,
    pub secrets_master_kid: String,
    pub secrets_retired_keys: Redacted<String>,
    pub internal_bind_addr: String,
    pub job_env_exchange_rate_per_min: u64,

    #[allow(dead_code)]
    #[allow(dead_code)]
    #[allow(dead_code)]
    #[allow(dead_code)]
    #[allow(dead_code)]
    pub metrics_include_tenant_label: bool,

    pub ingress_base_domain: Option<String>,

    #[allow(dead_code)]
    pub log_format: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("migration_database_url", &"<redacted>")
            .field("redis_url", &"<redacted>")
            .field("bootstrap_admin_token", &self.bootstrap_admin_token)
            .field("bind_addr", &self.bind_addr)
            .field("s3_endpoint", &self.s3_endpoint)
            .field("s3_region", &self.s3_region)
            .field("s3_bucket", &self.s3_bucket)
            .field("s3_access_key", &self.s3_access_key)
            .field("s3_secret_key", &self.s3_secret_key)
            .field("job_signing_key", &self.job_signing_key)
            .field("job_signing_kid", &self.job_signing_kid)
            .field("secrets_master_key", &self.secrets_master_key)
            .field("secrets_master_kid", &self.secrets_master_kid)
            .field("secrets_retired_keys", &self.secrets_retired_keys)
            .field("internal_bind_addr", &self.internal_bind_addr)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn bootstrap_admin_token_plain(&self) -> &str {
        self.bootstrap_admin_token.expose()
    }

    pub fn s3_secret_key_plain(&self) -> &str {
        self.s3_secret_key.expose()
    }

    pub fn job_signing_key_plain(&self) -> &str {
        self.job_signing_key.expose()
    }

    pub fn secret_keyring(&self) -> anyhow::Result<crate::secrets::SecretKeyring> {
        parse_secret_keyring(
            &self.secrets_master_kid,
            self.secrets_master_key.expose(),
            self.secrets_retired_keys.expose(),
        )
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let secrets_master_key = env_required("SECRETS_MASTER_KEY")?;
        if secrets_master_key.trim() == SECRETS_MASTER_KEY_PLACEHOLDER {
            anyhow::bail!(
                "SECRETS_MASTER_KEY is still the placeholder from .env.example; \
                 generate a real 32-byte key (e.g. `openssl rand -hex 32`) before starting"
            );
        }

        let endpoint = env_required("WORKER_HTTP_URL")?;
        let url = reqwest::Url::parse(&endpoint)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "WORKER_HTTP_URL must be an HTTP(S) URL"
        );
        let database_url = env_required("DATABASE_URL")?;
        let migration_database_url =
            env_optional("MIGRATION_DATABASE_URL").unwrap_or_else(|| database_url.clone());
        let cfg = Self {
            database_url,
            migration_database_url,
            bootstrap_admin_token: Redacted::new(env_required("BOOTSTRAP_ADMIN_TOKEN")?),
            bind_addr: env_or("BIND_ADDR", "0.0.0.0:8080"),

            s3_endpoint: env_or("S3_ENDPOINT", "http://127.0.0.1:9000"),
            s3_region: env_or("S3_REGION", "us-east-1"),
            s3_bucket: env_or("S3_BUCKET", "hibana-components"),
            s3_access_key: env_or("S3_ACCESS_KEY", "minioadmin"),
            s3_secret_key: Redacted::new(env_or("S3_SECRET_KEY", "minioadmin")),

            max_wasm_upload_bytes: env_u64("MAX_WASM_UPLOAD_BYTES", DEFAULT_MAX_WASM_UPLOAD_BYTES)?,
            presign_ttl_secs: env_u64("PRESIGN_TTL_SECS", DEFAULT_PRESIGN_TTL_SECS)?,

            job_signing_key: Redacted::new(env_required("JOB_SIGNING_KEY")?),
            job_signing_kid: env_required("JOB_SIGNING_KID")?,
            token_margin_secs: env_u64("TOKEN_MARGIN_SECS", 60)?,

            redis_url: env_or("REDIS_URL", DEFAULT_REDIS_URL),
            invoke_rate_per_sec: env_u64("QUOTA_INVOKE_RATE_PER_SEC", DEFAULT_INVOKE_RATE_PER_SEC)?,
            invoke_burst: env_u64("QUOTA_INVOKE_BURST", DEFAULT_INVOKE_BURST)?,
            max_concurrent_executions: env_u64(
                "QUOTA_MAX_CONCURRENT_EXECUTIONS",
                DEFAULT_MAX_CONCURRENT,
            )?,
            inflight_ttl_secs: env_u64("INFLIGHT_TTL_SECS", DEFAULT_INFLIGHT_TTL_SECS)?,
            login_lockout_threshold: env_u64(
                "LOGIN_LOCKOUT_THRESHOLD",
                DEFAULT_LOGIN_LOCKOUT_THRESHOLD,
            )?,
            login_lockout_window_secs: env_u64(
                "LOGIN_LOCKOUT_WINDOW_SECS",
                DEFAULT_LOGIN_LOCKOUT_WINDOW_SECS,
            )?,
            reaper_interval_secs: env_u64("REAPER_INTERVAL_SECS", DEFAULT_REAPER_INTERVAL_SECS)?,
            stuck_execution_deadline_secs: env_u64(
                "STUCK_EXECUTION_DEADLINE_SECS",
                DEFAULT_STUCK_EXECUTION_DEADLINE_SECS,
            )?,
            trust_proxy_headers: env_bool("TRUST_PROXY_HEADERS", false),

            secrets_master_key: Redacted::new(secrets_master_key),
            secrets_master_kid: env_required("SECRETS_MASTER_KID")?,
            secrets_retired_keys: Redacted::new(env_or("SECRETS_RETIRED_KEYS", "")),
            internal_bind_addr: env_or("INTERNAL_BIND_ADDR", DEFAULT_INTERNAL_BIND_ADDR),
            job_env_exchange_rate_per_min: env_u64(
                "JOB_ENV_EXCHANGE_RATE_PER_MIN",
                DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN,
            )?,
            metrics_include_tenant_label: env_bool(
                "METRICS_INCLUDE_TENANT_LABEL",
                DEFAULT_METRICS_INCLUDE_TENANT_LABEL,
            ),
            log_format: env_or("LOG_FORMAT", "text"),
            ingress_base_domain: env_optional("INGRESS_BASE_DOMAIN")
                .map(|s| s.trim().trim_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty()),
        };

        Ok(cfg)
    }

    /// Wall-clock execution limit plus time for cold compilation and result persistence.
    pub fn token_exp_offset_secs(&self, wall_time_ms: u64) -> i64 {
        wall_time_ms
            .div_ceil(1000)
            .saturating_add(self.token_margin_secs)
            .saturating_add(120)
            .min(i64::MAX as u64) as i64
    }

    pub fn admission(&self) -> crate::state::AdmissionConfig {
        use crate::store::{InflightParams, LockoutParams, RateLimitParams};
        crate::state::AdmissionConfig {
            rate: RateLimitParams {
                refill_per_sec: self.invoke_rate_per_sec as f64,
                capacity: self.invoke_burst as f64,
            },
            inflight: InflightParams {
                max: self.max_concurrent_executions as i64,
                ttl_secs: self.inflight_ttl_secs,
            },
            lockout: LockoutParams {
                threshold: self.login_lockout_threshold,
                window_secs: self.login_lockout_window_secs,
            },
            trust_proxy_headers: self.trust_proxy_headers,
        }
    }
}

fn env_required(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("required env var {key} is not set"))
}

fn env_optional(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| default.to_string())
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
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

pub(crate) fn parse_secret_keyring(
    active_kid: &str,
    active_raw: &str,
    retired_raw: &str,
) -> anyhow::Result<crate::secrets::SecretKeyring> {
    let active = crate::signing::decode_key32(active_raw, "SECRETS_MASTER_KEY")?;

    let mut retired = Vec::new();
    for entry in retired_raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (kid, raw) = entry.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("SECRETS_RETIRED_KEYS entries must be 'kid:key' (comma separated)")
        })?;
        let kid = kid.trim();
        if kid.is_empty() {
            anyhow::bail!("SECRETS_RETIRED_KEYS: empty kid");
        }
        retired.push((
            kid.to_string(),
            crate::signing::decode_key32(raw.trim(), "SECRETS_RETIRED_KEYS")?,
        ));
    }

    Ok(crate::secrets::SecretKeyring::new(
        active_kid.to_owned(),
        active,
        retired,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admitted_default_traffic_can_redeem_secrets() {
        use crate::store::{InProcStore, RateLimitParams, Store};
        let store = InProcStore::new();
        let public = RateLimitParams {
            refill_per_sec: DEFAULT_INVOKE_RATE_PER_SEC as f64,
            capacity: DEFAULT_INVOKE_BURST as f64,
        };
        let internal = RateLimitParams {
            refill_per_sec: DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN as f64 / 60.0,
            capacity: DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN as f64,
        };
        // Initial public burst, then two minutes at the admitted steady rate.
        // The previous 600/min internal default exhausted while public admission
        // still succeeded, turning otherwise valid Secret-backed HTTP into 502s.
        for now in
            std::iter::repeat_n(0, DEFAULT_INVOKE_BURST as usize).chain((20..=120_000).step_by(20))
        {
            assert!(
                store
                    .rate_limit("public", public, now)
                    .await
                    .unwrap()
                    .allowed
            );
            for key in ["env-peer", "env-tenant"] {
                assert!(store.rate_limit(key, internal, now).await.unwrap().allowed);
            }
        }
    }

    #[test]
    fn debug_never_reveals_secrets() {
        let secret_like = "SENTINEL-DO-NOT-LOG";
        let wrapped = Redacted::new(secret_like.to_string());
        let rendered = format!("{wrapped:?}");
        assert!(
            !rendered.contains(secret_like),
            "config secrets must never render in Debug output: {rendered}"
        );
        assert_eq!(rendered, "<redacted>");
    }
}
