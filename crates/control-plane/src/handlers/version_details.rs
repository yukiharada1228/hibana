use axum::{
    extract::{Path, State},
    Json,
};
use hibana_database::prelude::*;
use hibana_shared::{capabilities::parse_capabilities, FaasError};
use serde::Serialize;
use serde_json::Value;

use crate::{auth::Principal, db, error::AppError, state::AppState};

#[derive(Serialize)]
pub struct VersionDetailsResponse {
    version_id: String,
    wasm_sha256: String,
    build_metadata: Option<Value>,
    net_allow_outbound: Vec<String>,
}

/// Read-only details for one immutable version; no environment or Secret values.
pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
) -> Result<Json<VersionDetailsResponse>, AppError> {
    details(
        &state,
        &principal,
        &component_id,
        db::VersionRef::Name(&version),
    )
    .await
}

pub async fn get_by_id(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version_id)): Path<(String, String)>,
) -> Result<Json<VersionDetailsResponse>, AppError> {
    details(
        &state,
        &principal,
        &component_id,
        db::VersionRef::Id(&version_id),
    )
    .await
}

async fn details(
    state: &AppState,
    principal: &Principal,
    component_id: &str,
    version: db::VersionRef<'_>,
) -> Result<Json<VersionDetailsResponse>, AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &principal.tenant_id).await?;
    db::find_component_by_id(&tx, &principal.tenant_id, component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let details = db::version_details(&tx, &principal.tenant_id, component_id, version)
        .await?
        .ok_or_else(|| FaasError::NotFound("version".into()))?;
    tx.commit().await?;
    Ok(Json(VersionDetailsResponse {
        version_id: details.version_id,
        wasm_sha256: details.wasm_sha256,
        build_metadata: details.build_metadata,
        net_allow_outbound: parse_capabilities(&details.capabilities)
            .net_allow_outbound
            .into_iter()
            .collect(),
    }))
}
