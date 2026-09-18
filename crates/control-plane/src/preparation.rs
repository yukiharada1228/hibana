//! Compile before publication and reconcile active artifacts onto newly discovered Workers.
//! No guest handler runs during preparation. No database locks span Worker calls.
use crate::{db, dispatch, error::AppError, state::AppState};
use axum::{extract::State, http::HeaderMap, Json};
use futures::{stream, StreamExt};
use hibana_shared::{
    preparation::{Artifact, Claims, TOKEN_HEADER},
    FaasError,
};
use std::time::Duration;

fn endpoint() -> Result<String, std::env::VarError> {
    // Kubernetes preparation discovers cold Pods too; execution uses only ready
    // Pods. A single endpoint remains sufficient outside Kubernetes.
    match std::env::var("WORKER_PREPARATION_URL") {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => std::env::var("WORKER_HTTP_URL"),
    }
}

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
    let Ok(endpoint) = endpoint() else {
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
    let endpoint = endpoint().map_err(|_| FaasError::Unavailable)?;
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
    let artifacts = hibana_database::queries::active_artifacts(state.pool()).await?;
    for check_only in [false, true] {
        for artifact in &artifacts {
            prepare_targets(
                state,
                &artifact.tenant_id,
                &artifact.storage_uri,
                &artifact.sha256,
                &targets,
                check_only,
            )
            .await?;
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
) -> Result<crate::artifact_reservations::Reservation, AppError> {
    use hibana_database::prelude::*;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let row = hibana_database::queries::live_versions(tenant)
        .filter(component_versions::Column::Id.eq(version))
        .select_only()
        .columns([
            component_versions::Column::StorageUri,
            component_versions::Column::WasmSha256,
        ])
        .into_tuple::<(String, String)>()
        .one(&tx)
        .await?
        .ok_or_else(|| FaasError::NotFound("version".into()))?;
    tx.commit().await?;
    let mut reservation =
        crate::artifact_reservations::Reservation::new(state, tenant, version, &row.0, &row.1)
            .await?;
    reservation.confirm_object();
    prepare(state, tenant, &row.0, &row.1).await?;
    Ok(reservation)
}

async fn reconcile(state: &AppState) -> Result<(), AppError> {
    for artifact in hibana_database::queries::active_artifacts(state.pool()).await? {
        if prepare(
            state,
            &artifact.tenant_id,
            &artifact.storage_uri,
            &artifact.sha256,
        )
        .await
        .is_err()
        {
            tracing::warn!("Active artifact preparation incomplete; will retry");
        }
    }
    Ok(())
}

pub(crate) fn spawn(state: AppState) {
    if endpoint().is_err() {
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
