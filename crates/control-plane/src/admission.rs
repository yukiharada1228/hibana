//! Shared HTTP admission. Redis errors reject requests with 503; quotas return 429.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::{AppState, ResolvedAdmissionParams};
use crate::store::StoreError;

const CONCURRENCY_RETRY_AFTER_SECS: u64 = 1;

#[derive(Debug, Clone, Copy)]
pub struct RateLimited {
    pub retry_after_secs: u64,
    status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
}

impl RateLimited {
    pub fn unavailable() -> Self {
        Self {
            retry_after_secs: 1,
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "admission_unavailable",
            message: "shared admission store unavailable",
        }
    }

    pub fn rate(retry_after_secs: u64) -> Self {
        Self {
            retry_after_secs,
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: "invoke rate limit exceeded; retry later",
        }
    }

    pub fn concurrency() -> Self {
        Self {
            retry_after_secs: CONCURRENCY_RETRY_AFTER_SECS,
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "concurrency_limit",
            message: "max concurrent executions reached; retry later",
        }
    }
}

impl IntoResponse for RateLimited {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "retryable": true,
            }
        }));
        let mut resp = (self.status, body).into_response();
        if self.retry_after_secs > 0 {
            if let Ok(v) = header::HeaderValue::from_str(&self.retry_after_secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

pub enum Decision {
    Admitted,
    Rejected(RateLimited),
}

async fn fail_closed_degraded(state: &AppState, tenant: &str, class: &str, err: &StoreError) {
    tracing::warn!(
        tenant,
        class,
        "shared admission unavailable; rejecting HTTP request"
    );
    audit_degraded(state, tenant, class, err.is_unavailable()).await;
}

async fn audit_degraded(state: &AppState, tenant: &str, class: &str, unavailable: bool) {
    let detail = json!({
        "class": class,
        "fail_mode": "closed",
        "reason": if unavailable { "store_unavailable" } else { "store_backend_error" },
    });
    let res: anyhow::Result<()> = async {
        let mut tx = state.pool().begin().await?;
        crate::db::set_tenant_guc(&mut tx, tenant).await?;
        crate::db::insert_audit_log(
            &mut *tx,
            tenant,
            None,
            "admission_degraded",
            None,
            Some(&detail),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, class, "failed to write admission_degraded audit row");
    }
}

pub async fn check_rate_limit(
    state: &AppState,
    tenant: &str,
    params: &ResolvedAdmissionParams,
    now_ms: u64,
) -> Decision {
    match state.store().rate_limit(tenant, params.rate, now_ms).await {
        Ok(d) if d.allowed => Decision::Admitted,
        Ok(d) => Decision::Rejected(RateLimited::rate(d.retry_after_secs)),
        Err(e) => {
            fail_closed_degraded(state, tenant, "invoke_rate", &e).await;
            Decision::Rejected(RateLimited::unavailable())
        }
    }
}

pub async fn reserve_inflight(
    state: &AppState,
    tenant: &str,
    params: &ResolvedAdmissionParams,
) -> (Decision, bool) {
    match state
        .store()
        .reserve_inflight(tenant, params.inflight)
        .await
    {
        Ok(d) if d.admitted => (Decision::Admitted, true),
        Ok(_) => (Decision::Rejected(RateLimited::concurrency()), false),
        Err(e) => {
            fail_closed_degraded(state, tenant, "inflight", &e).await;
            (Decision::Rejected(RateLimited::unavailable()), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn rate_limited_sets_retry_after_and_envelope() {
        let resp = RateLimited::rate(7).into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).unwrap(),
            &header::HeaderValue::from_static("7")
        );
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "rate_limited");
        assert_eq!(v["error"]["retryable"], true);
    }

    #[tokio::test]
    async fn concurrency_limit_includes_retry_after() {
        let resp = RateLimited::concurrency().into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = resp.headers().get(header::RETRY_AFTER).expect(
            "concurrency_limit must include Retry-After per §8 MUST: \
             '429 で拒否し、Retry-After を付す'",
        );
        assert_eq!(header, &header::HeaderValue::from_static("1"));
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "concurrency_limit");
        assert_eq!(v["error"]["retryable"], true);
    }

    #[test]
    fn retry_after_is_passed_through_in_seconds() {
        assert_eq!(RateLimited::rate(0).retry_after_secs, 0);
        assert_eq!(RateLimited::rate(1).retry_after_secs, 1);
        assert_eq!(RateLimited::rate(3600).retry_after_secs, 3600);
    }

    #[tokio::test]
    async fn all_429_paths_include_retry_after_and_429_status() {
        for r in [RateLimited::rate(7), RateLimited::concurrency()] {
            let resp = r.into_response();
            assert_eq!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "all admission rejection paths must return 429 (§8 MUST)"
            );
            assert!(
                resp.headers().contains_key(header::RETRY_AFTER),
                "all 429 responses must include Retry-After (§8 MUST)"
            );
            let v = resp.headers().get(header::RETRY_AFTER).unwrap();
            let v = v.to_str().unwrap();
            assert!(
                v.parse::<u64>().is_ok(),
                "Retry-After must be a delta-seconds integer per RFC 7231; got {v:?}"
            );
        }
    }

    #[tokio::test]
    async fn rate_retry_after_is_taken_from_token_bucket() {
        let resp = RateLimited::rate(42).into_response();
        let v = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("rate-limited 429 must include Retry-After");
        assert_eq!(v, &header::HeaderValue::from_static("42"));
    }
}
