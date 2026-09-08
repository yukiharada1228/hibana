//! Synchronous HTTP dispatch. Never replay an ambiguous HTTP request automatically.
//! A Worker redeems a signed token for the pinned server-side job. Pending-only
//! cancellation and the Worker claim race atomically in PostgreSQL.
use crate::{db, error::AppError, state::AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use hibana_shared::FaasError;

/// Reuse the signed, idempotent finalizer over HTTP. NATS is not on the
/// synchronous response path; transient retries repeat the result, never the guest.
pub async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(result): Json<hibana_shared::ResultMessage>,
) -> Result<StatusCode, AppError> {
    let claims = crate::job_auth::claims_from_token(&state, &headers).await?;
    if claims.tenant_id != result.tenant_id
        || claims.execution_id != result.execution_id
        || headers
            .get("x-hibana-job-token")
            .and_then(|v| v.to_str().ok())
            != Some(result.job_token.as_str())
        || !result.status.is_terminal()
    {
        return Err(FaasError::Unauthorized.into());
    }
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &claims.tenant_id).await?;
    let row = sqlx::query("SELECT e.status FROM executions e WHERE e.tenant_id=$1 AND e.id=$2 AND e.version_id=$3 AND e.http_request")
        .bind(&claims.tenant_id).bind(&claims.execution_id).bind(&claims.version_id).fetch_optional(&mut *tx).await?.ok_or(FaasError::Unauthorized)?;
    let status: String = row.get("status");
    if status == result.status.as_str() {
        return Ok(StatusCode::NO_CONTENT);
    }
    tx.commit().await?;
    let payload = serde_json::to_vec(&result).map_err(FaasError::Serialization)?;
    crate::completion::handle_message(&state, &claims.tenant_id, &payload)
        .await
        .map_err(|_| FaasError::Internal("HTTP result finalization failed".into()))?;
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &claims.tenant_id).await?;
    let saved = db::get_execution(&mut *tx, &claims.tenant_id, &claims.execution_id)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    tx.commit().await?;
    if saved.status != result.status.as_str() {
        return Err(FaasError::Conflict("HTTP result rejected".into()).into());
    }
    Ok(StatusCode::NO_CONTENT)
}
use serde_json::Value;
use sqlx::Row;

pub async fn redeem(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<hibana_shared::JobMessage>, AppError> {
    let claims = crate::job_auth::claims_from_token(&state, &headers).await?;
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &claims.tenant_id).await?;
    let row = sqlx::query("SELECT e.component_id,e.input,c.name,v.version,v.wasm_sha256,v.resource_limits FROM executions e JOIN components c ON c.id=e.component_id AND c.tenant_id=e.tenant_id JOIN component_versions v ON v.id=e.version_id AND v.tenant_id=e.tenant_id AND v.component_id=e.component_id WHERE e.tenant_id=$1 AND e.id=$2 AND e.version_id=$3 AND e.status='pending' AND e.http_request AND c.deleted_at IS NULL AND v.deleted_at IS NULL")
        .bind(&claims.tenant_id).bind(&claims.execution_id).bind(&claims.version_id)
        .fetch_optional(&mut *tx).await?.ok_or(FaasError::Unauthorized)?;
    let limits: hibana_shared::ResourceLimits =
        serde_json::from_value(row.get::<Value, _>("resource_limits"))
            .map_err(FaasError::Serialization)?;
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(limits.max_wall_time_ms);
    let component_id: String = row.get("component_id");
    let needs_env = db::version_has_live_secrets(
        &mut *tx,
        &claims.tenant_id,
        &component_id,
        &claims.version_id,
    )
    .await?;
    let env_token = needs_env.then(|| {
        state.signer().sign_env(&hibana_shared::EnvClaims {
            execution_id: claims.execution_id.clone(),
            tenant_id: claims.tenant_id.clone(),
            version_id: claims.version_id.clone(),
            component_id,
            aud: hibana_shared::ENV_TOKEN_AUDIENCE.into(),
            kid: state.signer().kid().into(),
            iat,
            exp,
        })
    });
    let job_token = state.signer().sign(&hibana_shared::JobClaims {
        iat,
        exp,
        kid: state.signer().kid().into(),
        ..claims.clone()
    });
    let job = hibana_shared::JobMessage {
        execution_id: claims.execution_id,
        tenant_id: claims.tenant_id,
        component: row.get("name"),
        version: row.get("version"),
        wasm_sha256: row.get("wasm_sha256"),
        wasm_url: String::new(),
        input: row.get("input"),
        job_token,
        env_token,
    };
    tx.commit().await?;
    Ok(Json(job))
}

async fn forward(
    state: &AppState,
    tenant: &str,
    id: &str,
    version_id: &str,
) -> Result<Response, AppError> {
    use tracing::Instrument;
    let started = std::time::Instant::now();
    let iat = chrono::Utc::now().timestamp();
    let token = state.signer().sign(&hibana_shared::JobClaims {
        execution_id: id.into(),
        tenant_id: tenant.into(),
        version_id: version_id.into(),
        kid: state.signer().kid().into(),
        iat,
        exp: iat + 120,
    });
    let endpoint = std::env::var("WORKER_HTTP_URL")
        .map_err(|_| FaasError::Internal("HTTP pool is not configured".into()))?;
    tracing::debug!(target: "hibana_latency", execution_id = %id, stage = "cp_forward_setup",
        elapsed_us = started.elapsed().as_micros() as u64, "HTTP phase timing");
    let response = match crate::dispatch::send(state.worker_http(), &endpoint, &token)
        .instrument(tracing::debug_span!(target: "hibana_latency", "dispatch", execution_id = %id))
        .await
    {
        Ok(response) => response,
        Err(_) => {
            let cancelled = cancel_pending(state, tenant, id).await?;
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(if cancelled {
                    "Worker unavailable; invocation did not start"
                } else {
                    "Worker unavailable; invocation may have started"
                }))
                .unwrap());
        }
    };
    if !response.status().is_success() {
        // A guest error has already claimed the row, so this CAS cannot cancel it.
        cancel_pending(state, tenant, id).await?;
    }
    let mut builder = Response::builder().status(response.status());
    let connection = response
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for (name, value) in response.headers() {
        if !hop_header(name.as_str(), connection) {
            builder = builder.header(name, value);
        }
    }
    Ok(builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap())
}

fn hop_header(name: &str, connection: &str) -> bool {
    name == hibana_shared::http::WORKER_REJECTED_HEADER
        || matches!(
            name,
            "connection"
                | "transfer-encoding"
                | "content-length"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "upgrade"
        )
        || connection
            .split(',')
            .any(|v| v.trim().eq_ignore_ascii_case(name))
}

/// Admit one HTTP request and pin its version before contacting a Worker.
/// A transport failure never causes replay; cancel only if no Worker claimed the row.
pub async fn accept(
    state: &AppState,
    tenant: &str,
    component: &str,
    input: Value,
) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    use crate::admission::{self, Decision};
    use axum::response::IntoResponse;
    let label = if state.metrics_include_tenant_label() {
        tenant
    } else {
        "aggregate"
    };
    state
        .metrics()
        .tenant_invoke_total
        .with_label_values(&[label])
        .inc();
    let (status, quotas) = db::load_tenant_status_and_quotas(state.pool(), tenant)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    if status != "active" {
        return Err(FaasError::Unauthorized.into());
    }
    let resolved = state.admission().resolve_for_tenant(tenant, &quotas);
    if let Decision::Rejected(r) =
        admission::check_rate_limit(state, tenant, &resolved, crate::store::now_unix_millis()).await
    {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&[r.code])
            .inc();
        return Ok(r.into_response());
    }
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = sqlx::query("SELECT c.id AS component_id,v.id AS version_id FROM components c JOIN component_versions v ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id WHERE c.tenant_id=$1 AND c.name=$2 AND c.deleted_at IS NULL AND c.ingress_enabled AND v.deleted_at IS NULL FOR SHARE OF c")
        .bind(tenant).bind(component).fetch_optional(&mut *tx).await?.ok_or_else(|| FaasError::NotFound("HTTP application".into()))?;
    // Serialize only acceptance for this tenant, not guest execution. Redis can
    // temporarily undercount during reconciliation; the committed DB records
    // are the authoritative concurrency bound across Control Plane instances.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("hibana:http-admission:{tenant}"))
        .execute(&mut *tx)
        .await?;
    if db::count_inflight_executions(&mut *tx, tenant).await? >= resolved.inflight.max {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["concurrency_limit"])
            .inc();
        return Ok(admission::RateLimited::concurrency().into_response());
    }
    let (decision, reserved) = admission::reserve_inflight(state, tenant, &resolved).await;
    if let Decision::Rejected(r) = decision {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&[r.code])
            .inc();
        return Ok(r.into_response());
    }
    let id = hibana_shared::new_execution_id();
    let version_id: String = row.get("version_id");
    let inserted: Result<(), sqlx::Error> = async {
        sqlx::query("INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,job_token_kid,routing_reason,http_request) VALUES ($1,$2,$3,$4,'pending',$5,$6,'stable',true)")
            .bind(&id).bind(tenant).bind(row.get::<String,_>("component_id")).bind(&version_id)
            .bind(&input).bind(state.signer().kid()).execute(&mut *tx).await?;
        tx.commit().await
    }.await;
    if let Err(error) = inserted {
        if reserved {
            let _ = state.store().release_inflight(tenant).await;
        }
        return Err(error.into());
    }
    let guard = PendingDispatch {
        state: state.clone(),
        tenant: tenant.into(),
        id: id.clone(),
        armed: true,
    };
    tracing::debug!(target: "hibana_latency", execution_id = %id, stage = "cp_admission",
        elapsed_us = started.elapsed().as_micros() as u64, "HTTP phase timing");
    // The committed acceptance already pinned this version; do not read it again.
    let result = forward(state, tenant, &id, &version_id).await;
    let mut guard = guard;
    if result.is_err() {
        cancel_pending(state, tenant, &id).await?;
    }
    guard.armed = false;
    result
}

/// The same pending predicate as the Worker claim prevents cancelling an execution
/// that may have performed side effects. Only the winning transition releases Redis.
/// Rejected dispatches are not guest invocations and do not accrue usage.
pub(crate) async fn cancel_pending(
    state: &AppState,
    tenant: &str,
    id: &str,
) -> Result<bool, AppError> {
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let changed = sqlx::query("UPDATE executions SET status='failed',finished_at=now(),error=$3 WHERE tenant_id=$1 AND id=$2 AND http_request AND status='pending'")
        .bind(tenant).bind(id).bind(serde_json::json!({"code":"dispatch_not_started","message":"No Worker claimed the request"}))
        .execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    if changed {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["worker_unavailable"])
            .inc();
        if let Err(error) = state.store().release_inflight(tenant).await {
            tracing::warn!(execution_id = %id, %error, "dispatch cancellation counter release failed; reaper will reconcile");
        }
    }
    Ok(changed)
}

struct PendingDispatch {
    state: AppState,
    tenant: String,
    id: String,
    armed: bool,
}
impl Drop for PendingDispatch {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Axum may drop acceptance on client disconnect after the DB commit.
        let (state, tenant, id) = (self.state.clone(), self.tenant.clone(), self.id.clone());
        tokio::spawn(async move {
            if let Err(error) = cancel_pending(&state, &tenant, &id).await {
                tracing::warn!(execution_id = %id, error = %error.0, "abandoned dispatch cancellation failed; reaper will reconcile");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn connection_nominated_headers_are_not_forwarded() {
        assert!(super::hop_header("x-private", "keep-alive, X-Private"));
        assert!(super::hop_header("transfer-encoding", ""));
        assert!(!super::hop_header("set-cookie", ""));
    }
}
