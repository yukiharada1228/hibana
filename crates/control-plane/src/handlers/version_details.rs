use axum::{
    extract::{Path, State},
    Json,
};
use hibana_database::prelude::*;
use hibana_shared::FaasError;

use crate::{auth::Principal, db, error::AppError, state::AppState};

/// Read-only details for one immutable version; no environment or Secret values.
pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
) -> Result<Json<db::VersionDetails>, AppError> {
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
) -> Result<Json<db::VersionDetails>, AppError> {
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
) -> Result<Json<db::VersionDetails>, AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &principal.tenant_id).await?;
    let details = db::version_details(&tx, &principal.tenant_id, component_id, version)
        .await?
        .ok_or_else(|| FaasError::NotFound("version".into()))?;
    tx.commit().await?;
    Ok(Json(details))
}
