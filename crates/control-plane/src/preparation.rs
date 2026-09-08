//! Compile before publication and reconcile active artifacts onto newly discovered Workers.
//! No guest handler runs during preparation. No database locks span Worker calls.
use crate::{db, dispatch, error::AppError, state::AppState};
use axum::{extract::State, http::HeaderMap, Json};
use futures::{stream, StreamExt};
use hibana_shared::{
    preparation::{Artifact, Claims, TOKEN_HEADER},
    FaasError,
};
use sqlx::Row;
use std::time::Duration;

pub(crate) async fn redeem(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Artifact>, AppError> {
    let token = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(FaasError::Unauthorized)?;
    let claims = state
        .signer()
        .verifier()
        .verify_preparation(token)
        .map_err(|_| FaasError::Unauthorized)?;
    if !claims.valid_at(chrono::Utc::now().timestamp()) {
        return Err(FaasError::Unauthorized.into());
    }
    if !db::tenant_is_active(state.pool(), &claims.tenant_id).await? {
        return Err(FaasError::Forbidden.into());
    }
    let url = state
        .storage()
        .presign_get(&claims.storage_uri, state.presign_ttl())
        .await?;
    Ok(Json(Artifact {
        sha256: claims.sha256,
        url,
    }))
}

pub(crate) async fn prepare(
    state: &AppState,
    tenant: &str,
    storage_uri: &str,
    sha256: &str,
) -> Result<(), AppError> {
    let Ok(endpoint) = std::env::var("WORKER_HTTP_URL") else {
        // Management-only processes can store versions without an execution fleet.
        return Ok(());
    };
    let targets = dispatch::discover(&endpoint, "prepare")
        .await
        .map_err(|_| FaasError::Unavailable)?;
    prepare_targets(state, tenant, storage_uri, sha256, &targets, false).await
}

async fn prepare_targets(
    state: &AppState,
    tenant: &str,
    storage_uri: &str,
    sha256: &str,
    targets: &[reqwest::Url],
    check_only: bool,
) -> Result<(), AppError> {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        tenant_id: tenant.into(),
        storage_uri: storage_uri.into(),
        sha256: sha256.into(),
        kid: state.signer().kid().into(),
        iat: now,
        exp: now + 120,
    };
    if !claims.valid_at(now) {
        return Err(FaasError::InvalidRequest("Invalid preparation artifact".into()).into());
    }
    let token = state.signer().sign_preparation(&claims);
    let results = tokio::time::timeout(
        Duration::from_secs(90),
        stream::iter(targets.to_vec())
            .map(|target| {
                prepare_target(
                    state.worker_http().clone(),
                    token.clone(),
                    target,
                    check_only,
                )
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(|_| FaasError::Unavailable)?;
    for result in results {
        result?;
    }
    Ok(())
}

// Each concurrent future owns its request data. Borrowed async closures here
// prevent Send from being proven through buffer_unordered on supported Rust builds.
async fn prepare_target(
    client: reqwest::Client,
    token: String,
    target: reqwest::Url,
    check_only: bool,
) -> Result<(), FaasError> {
    loop {
        let response = client
            .request(
                if check_only {
                    reqwest::Method::HEAD
                } else {
                    reqwest::Method::POST
                },
                target.clone(),
            )
            .header(TOKEN_HEADER, &token)
            .timeout(Duration::from_secs(85))
            .send()
            .await
            .map_err(|_| FaasError::Unavailable)?;
        if response.status() == axum::http::StatusCode::NO_CONTENT {
            return Ok(());
        }
        if check_only || response.status() != axum::http::StatusCode::SERVICE_UNAVAILABLE {
            return Err(FaasError::Unavailable);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Maintenance barrier for a known ready fleet. The operator supplies Pod IPs
/// from Kubernetes so a stale/partial DNS answer cannot report success. No guest
/// code runs, and a final cache-only pass detects eviction during preparation.
pub(crate) async fn prepare_active(
    state: &AppState,
    expected: &std::collections::BTreeSet<std::net::IpAddr>,
) -> Result<(), AppError> {
    let endpoint = std::env::var("WORKER_HTTP_URL").map_err(|_| FaasError::Unavailable)?;
    let targets = dispatch::discover(&endpoint, "prepare")
        .await
        .map_err(|_| FaasError::Unavailable)?;
    let actual: std::collections::BTreeSet<std::net::IpAddr> = targets
        .iter()
        .filter_map(|target| target.host_str()?.trim_matches(['[', ']']).parse().ok())
        .collect();
    if expected.is_empty() || &actual != expected || targets.len() != expected.len() {
        return Err(FaasError::Unavailable.into());
    }
    let mut artifacts = Vec::new();
    for tenant in db::list_active_tenant_ids(state.pool()).await? {
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, &tenant).await?;
        let rows = sqlx::query("SELECT DISTINCT v.storage_uri,v.wasm_sha256 FROM components c JOIN component_versions v ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id WHERE c.tenant_id=$1 AND c.deleted_at IS NULL AND v.deleted_at IS NULL")
            .bind(&tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for row in rows {
            artifacts.push((
                tenant.clone(),
                row.get::<String, _>("storage_uri"),
                row.get::<String, _>("wasm_sha256"),
            ));
        }
    }
    for check_only in [false, true] {
        for (tenant, storage_uri, sha256) in &artifacts {
            prepare_targets(state, tenant, storage_uri, sha256, &targets, check_only).await?;
        }
    }
    if dispatch::discover(&endpoint, "prepare")
        .await
        .map_err(|_| FaasError::Unavailable)?
        != targets
    {
        return Err(FaasError::Unavailable.into());
    }
    Ok(())
}

pub(crate) async fn prepare_version(
    state: &AppState,
    tenant: &str,
    version: &str,
) -> Result<(), AppError> {
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = sqlx::query("SELECT v.storage_uri,v.wasm_sha256 FROM component_versions v JOIN components c ON c.id=v.component_id AND c.tenant_id=v.tenant_id WHERE v.tenant_id=$1 AND v.id=$2 AND v.deleted_at IS NULL AND c.deleted_at IS NULL")
        .bind(tenant).bind(version).fetch_optional(&mut *tx).await?.ok_or_else(|| FaasError::NotFound("version".into()))?;
    tx.commit().await?;
    prepare(
        state,
        tenant,
        &row.get::<String, _>("storage_uri"),
        &row.get::<String, _>("wasm_sha256"),
    )
    .await
}

async fn reconcile(state: &AppState) -> Result<(), AppError> {
    for tenant in db::list_active_tenant_ids(state.pool()).await? {
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, &tenant).await?;
        let rows = sqlx::query("SELECT DISTINCT v.storage_uri,v.wasm_sha256 FROM components c JOIN component_versions v ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id WHERE c.tenant_id=$1 AND c.deleted_at IS NULL AND v.deleted_at IS NULL")
            .bind(&tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for row in rows {
            if prepare(
                state,
                &tenant,
                &row.get::<String, _>("storage_uri"),
                &row.get::<String, _>("wasm_sha256"),
            )
            .await
            .is_err()
            {
                tracing::warn!("Active artifact preparation incomplete; will retry");
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn(state: AppState) {
    if std::env::var("WORKER_HTTP_URL").is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            if reconcile(&state).await.is_err() {
                tracing::warn!("Artifact preparation reconciliation unavailable");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
