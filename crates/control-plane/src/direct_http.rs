//! Synchronous HTTP dispatch. Never replay an ambiguous HTTP request automatically.
//! The accepted execution remains in the DB for accounting/reaping; it is excluded
//! from async dispatch. A Worker redeems a signed token for the pinned server-side job.
use crate::{db, error::AppError, state::AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use faas_shared::FaasError;

/// Reuse the signed, idempotent finalizer over HTTP. NATS is not on the
/// synchronous response path; transient retries repeat the result, never the guest.
pub async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(result): Json<faas_shared::ResultMessage>,
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
) -> Result<Json<faas_shared::JobMessage>, AppError> {
    let claims = crate::job_auth::claims_from_token(&state, &headers).await?;
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &claims.tenant_id).await?;
    let row = sqlx::query("SELECT e.component_id,e.input,c.name,v.version,v.wasm_sha256,v.storage_uri,v.resource_limits FROM executions e JOIN components c ON c.id=e.component_id AND c.tenant_id=e.tenant_id JOIN component_versions v ON v.id=e.version_id AND v.tenant_id=e.tenant_id AND v.component_id=e.component_id WHERE e.tenant_id=$1 AND e.id=$2 AND e.version_id=$3 AND e.status='pending' AND e.http_request AND c.deleted_at IS NULL AND v.deleted_at IS NULL")
        .bind(&claims.tenant_id).bind(&claims.execution_id).bind(&claims.version_id)
        .fetch_optional(&mut *tx).await?.ok_or(FaasError::Unauthorized)?;
    let limits: faas_shared::ResourceLimits =
        serde_json::from_value(row.get::<Value, _>("resource_limits"))
            .map_err(FaasError::Serialization)?;
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(limits.max_wall_time_ms);
    let component_id: String = row.get("component_id");
    let needs_env =
        db::component_has_live_secrets(&mut *tx, &claims.tenant_id, &component_id).await?;
    let env_token = needs_env.then(|| {
        state.signer().sign_env(&faas_shared::EnvClaims {
            execution_id: claims.execution_id.clone(),
            tenant_id: claims.tenant_id.clone(),
            version_id: claims.version_id.clone(),
            component_id,
            aud: faas_shared::ENV_TOKEN_AUDIENCE.into(),
            kid: state.signer().kid().into(),
            iat,
            exp,
        })
    });
    let job_token = state.signer().sign(&faas_shared::JobClaims {
        iat,
        exp,
        kid: state.signer().kid().into(),
        ..claims.clone()
    });
    let wasm_url = state
        .storage()
        .presign_get(&row.get::<String, _>("storage_uri"), state.presign_ttl())
        .await?;
    let job = faas_shared::JobMessage {
        execution_id: claims.execution_id,
        tenant_id: claims.tenant_id,
        component: row.get("name"),
        version: row.get("version"),
        wasm_sha256: row.get("wasm_sha256"),
        wasm_url,
        input: row.get("input"),
        job_token,
        env_token,
    };
    tx.commit().await?;
    Ok(Json(job))
}

pub async fn forward(state: &AppState, tenant: &str, id: &str) -> Result<Response, AppError> {
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = db::get_execution(&mut *tx, tenant, id)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    tx.commit().await?;
    let iat = chrono::Utc::now().timestamp();
    let token = state.signer().sign(&faas_shared::JobClaims {
        execution_id: id.into(),
        tenant_id: tenant.into(),
        version_id: row.version_id,
        kid: state.signer().kid().into(),
        iat,
        exp: iat + 120,
    });
    let endpoint = std::env::var("WORKER_HTTP_URL")
        .map_err(|_| FaasError::Internal("HTTP pool is not configured".into()))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(3))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|_| FaasError::Internal("Invalid HTTP client".into()))?;
    // No retry: a transport failure may have happened after an external side effect.
    let response = match client
        .post(format!("{}/invoke", endpoint.trim_end_matches('/')))
        .header("x-hibana-job-token", token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(
                    "Worker unavailable; invocation may have started",
                ))
                .unwrap())
        }
    };
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
    matches!(
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
    ) || connection
        .split(',')
        .any(|v| v.trim().eq_ignore_ascii_case(name))
}

/// Admit one HTTP request and pin its version before contacting a Worker.
/// A transport failure never causes replay; the reaper recovers abandoned reservations.
pub async fn accept(
    state: &AppState,
    tenant: &str,
    component: &str,
    input: Value,
) -> Result<Response, AppError> {
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
    let row = sqlx::query("SELECT c.id AS component_id,v.id AS version_id FROM components c JOIN component_versions v ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id WHERE c.tenant_id=$1 AND c.name=$2 AND c.deleted_at IS NULL AND c.ingress_enabled AND v.deleted_at IS NULL")
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
    let id = faas_shared::new_execution_id();
    let inserted: Result<(), sqlx::Error> = async {
        sqlx::query("INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,job_token_kid,routing_reason,http_request) VALUES ($1,$2,$3,$4,'pending',$5,$6,'stable',true)")
            .bind(&id).bind(tenant).bind(row.get::<String,_>("component_id")).bind(row.get::<String,_>("version_id"))
            .bind(&input).bind(state.signer().kid()).execute(&mut *tx).await?;
        tx.commit().await
    }.await;
    if let Err(error) = inserted {
        if reserved {
            let _ = state.store().release_inflight(tenant).await;
        }
        return Err(error.into());
    }
    forward(state, tenant, &id).await
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
