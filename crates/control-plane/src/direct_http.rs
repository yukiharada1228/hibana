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
use hibana_database::prelude::*;
use hibana_shared::FaasError;

use serde_json::Value;

pub async fn redeem(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<hibana_shared::JobMessage>, AppError> {
    let claims = crate::job_auth::claims_from_token(&state, &headers).await?;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &claims.tenant_id).await?;
    let row = hibana_database::queries::pinned_execution(&claims.tenant_id, &claims.execution_id)
        .filter(executions::Column::VersionId.eq(&claims.version_id))
        .select_only()
        .column(executions::Column::ComponentId)
        .column(executions::Column::Input)
        .column_as(
            Expr::col((components::Entity, components::Column::Name)),
            "name",
        )
        .column_as(
            Expr::col((
                component_versions::Entity,
                component_versions::Column::WasmSha256,
            )),
            "wasm_sha256",
        )
        .column_as(
            Expr::col((
                component_versions::Entity,
                component_versions::Column::ResourceLimits,
            )),
            "resource_limits",
        )
        .into_model::<RedeemedExecution>()
        .one(&tx)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    let limits: hibana_shared::ResourceLimits = serde_json::from_value(row.resource_limits)?;
    let iat = chrono::Utc::now().timestamp();
    let exp = iat.saturating_add(state.token_exp_offset_secs(limits.max_wall_time_ms));
    let component_id: String = row.component_id;
    let needs_env =
        db::version_has_live_secrets(&tx, &claims.tenant_id, &component_id, &claims.version_id)
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
        component: row.name,
        wasm_sha256: row.wasm_sha256,
        input: row.input,
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
    input: impl std::future::Future<Output = Result<Value, Response>>,
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
    if let Decision::Rejected(r) = admission::check_rate_limit(state, tenant, &resolved).await {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&[r.code])
            .inc();
        return Ok(r.into_response());
    }
    let Some(_tenant_request) = state
        .request_capacity()
        .reserve_tenant(tenant, resolved.max_concurrent_executions as usize)
    else {
        return Ok(admission::RateLimited::concurrency().into_response());
    };
    let input = match input.await {
        Ok(input) => input,
        Err(response) => return Ok(response),
    };
    // Receiving can take seconds. Honor suspension/quota changes before creating
    // an execution, without charging the request's rate limit twice.
    let (status, quotas) = db::load_tenant_status_and_quotas(state.pool(), tenant)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    if status != "active" {
        return Err(FaasError::Unauthorized.into());
    }
    let resolved = state.admission().resolve_for_tenant(tenant, &quotas);
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    // Lock only the parent first. A join started before a publisher commits can
    // lose its old active-version match while waiting and incorrectly return 404.
    let component_id = components::Entity::find()
        .select_only()
        .column(components::Column::Id)
        .filter(components::Column::TenantId.eq(tenant))
        .filter(components::Column::Name.eq(component))
        .filter(components::Column::DeletedAt.is_null())
        .filter(components::Column::IngressEnabled.eq(true))
        .lock_shared()
        .into_tuple::<String>()
        .one(&tx)
        .await?
        .ok_or_else(|| FaasError::NotFound("HTTP application".into()))?;
    // This new statement sees the publisher's commit. Use the database clock
    // after the wait, rather than transaction-start now(), for Secret selection.
    let (version_id, accepted_at) = hibana_database::queries::active_versions(tenant)
        .filter(component_versions::Column::ComponentId.eq(&component_id))
        .select_only()
        .column(component_versions::Column::Id)
        .expr(Func::cust("clock_timestamp"))
        .into_tuple::<(String, chrono::DateTime<chrono::Utc>)>()
        .one(&tx)
        .await?
        .ok_or_else(|| FaasError::NotFound("HTTP application".into()))?;
    // Serialize acceptance for this tenant across Control Plane instances.
    // Counting and inserting in this transaction makes the DB the only source
    // of concurrency slots; completion releases a slot by committing its status.
    hibana_database::postgres::lock_tenant_admission(
        &tx,
        &format!("hibana:http-admission:{tenant}"),
    )
    .await?;
    if db::count_inflight_executions(&tx, tenant).await? >= resolved.max_concurrent_executions {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["concurrency_limit"])
            .inc();
        return Ok(admission::RateLimited::concurrency().into_response());
    }
    let id = hibana_shared::new_execution_id();
    executions::Entity::insert(executions::ActiveModel {
        id: Set(id.clone()),
        tenant_id: Set(tenant.into()),
        component_id: Set(component_id),
        version_id: Set(version_id.clone()),
        status: Set("pending".into()),
        input: Set(Some(input)),
        job_token_kid: Set(Some(state.signer().kid().into())),
        http_request: Set(true),
        created_at: Set(accepted_at),
        ..Default::default()
    })
    .exec_without_returning(&tx)
    .await?;
    tx.commit().await?;
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
/// that may have performed side effects. The committed status frees the DB slot.
/// Rejected dispatches are not guest invocations and do not accrue usage.
pub(crate) async fn cancel_pending(
    state: &AppState,
    tenant: &str,
    id: &str,
) -> Result<bool, AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let changed = executions::Entity::update_many()
        .col_expr(executions::Column::Status, Expr::val("failed"))
        .col_expr(executions::Column::Input, Expr::val(None::<Value>))
        .col_expr(executions::Column::InputRef, Expr::val(None::<String>))
        .col_expr(executions::Column::FinishedAt, now())
        .col_expr(
            executions::Column::Error,
            Expr::val(serde_json::json!({
                "code": "dispatch_not_started", "message": "No Worker claimed the request"
            })),
        )
        .filter(executions::Column::TenantId.eq(tenant))
        .filter(executions::Column::Id.eq(id))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(executions::Column::Status.eq("pending"))
        .exec(&tx)
        .await?
        .rows_affected
        == 1;
    tx.commit().await?;
    if changed {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["worker_unavailable"])
            .inc();
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

#[derive(FromQueryResult)]
struct RedeemedExecution {
    component_id: String,
    input: Value,
    name: String,
    wasm_sha256: String,
    resource_limits: Value,
}
