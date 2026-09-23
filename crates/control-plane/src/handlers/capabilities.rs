//! Capabilities management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::FaasError;
use sea_orm::TransactionTrait as _;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub struct GetCapabilitiesResponse {
    pub component_id: String,
    pub version: String,
    /// vars と許可済み Secret 参照から配備時に確定した環境変数名。
    pub env: BTreeSet<String>,
    /// 承認された egress 先（host:port, M9c）。
    pub net_allow_outbound: BTreeSet<String>,
}

/// GET /components/{id}/versions/{version}/capabilities — 現在の承認内容を返す（Read）。
///
/// 値は返さず、環境変数名と外向き通信の許可先だけを返す。
pub async fn get_capabilities(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let component = db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let version_id = db::find_version_id(&tx, tenant, &component_id, &version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("version '{version}' of component '{component_id}'"))
        })?;
    let current = db::version_capabilities(&tx, tenant, &version_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    Ok(Json(GetCapabilitiesResponse {
        component_id,
        version,
        env: hibana_shared::capabilities::allowed_env(&current),
        net_allow_outbound: super::egress::policy(&component)?,
    }))
}
