//! Read the active version's configuration and validate deployment vars.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_database::prelude::*;
use hibana_shared::FaasError;
use serde::Serialize;

// Config may contain sensitive plaintext, so reading it requires Deploy scope.
// Changes are published with a new version; this API has no write operations.

#[derive(Debug, Serialize)]
pub struct FunctionConfigResponse {
    pub component_id: String,
    pub version_id: Option<String>,
    pub resource_limits: Option<serde_json::Value>,
    pub net_allow_outbound: Vec<String>,
    pub secrets: Vec<SecretBindingResponse>,
    pub env: std::collections::BTreeMap<String, String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, FromQueryResult)]
pub struct SecretBindingResponse {
    pub name: String,
    pub available: bool,
}

/// config の入力（キー名・NUL・値長・件数・総バイト）を検証する純関数（DB / ストア非依存）。
///
/// 上限は `hibana_shared` の定数を使う（CP の受付と worker の防御的 clamp で同じ値を参照する
/// 二重防御）。超過は 400（`error.rs` は `InvalidRequest` を 400 にしか写像しない）。
pub(super) fn validate_env_map(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<(), FaasError> {
    if env.len() > hibana_shared::MAX_FUNCTION_ENV_KEYS {
        return Err(FaasError::InvalidRequest(format!(
            "at most {} env entries are allowed per component",
            hibana_shared::MAX_FUNCTION_ENV_KEYS
        )));
    }
    let mut total = 0usize;
    for (k, v) in env {
        if !hibana_shared::is_valid_env_key(k) {
            return Err(FaasError::InvalidRequest(format!(
                "invalid env key '{k}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
                hibana_shared::MAX_ENV_KEY_LEN - 1
            )));
        }
        // PostgreSQL text cannot store NUL. Reject before Wasm preparation and storage.
        if v.contains('\0') {
            return Err(FaasError::InvalidRequest(format!(
                "env value for '{k}' contains NUL (U+0000); remove NUL characters before deploying"
            )));
        }
        if v.len() > hibana_shared::MAX_ENV_VALUE_BYTES {
            return Err(FaasError::InvalidRequest(format!(
                "env value for '{k}' exceeds {} bytes",
                hibana_shared::MAX_ENV_VALUE_BYTES
            )));
        }
        total += k.len() + v.len();
    }
    if total > hibana_shared::MAX_FUNCTION_ENV_TOTAL_BYTES {
        return Err(FaasError::InvalidRequest(format!(
            "total env size exceeds {} bytes",
            hibana_shared::MAX_FUNCTION_ENV_TOTAL_BYTES
        )));
    }
    Ok(())
}

/// GET /components/{id}/config — 平文 config を返す（deploy）。
pub async fn get_function_config(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // Hold the publication lock so vars, bindings and limits describe one version.
    let component = components::Entity::find_by_id(component_id.clone())
        .filter(components::Column::TenantId.eq(tenant))
        .filter(components::Column::DeletedAt.is_null())
        .lock_shared()
        .one(&tx)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;
    let rows = db::list_function_configs(&tx, tenant, &component_id).await?;
    let mut secrets = Vec::new();
    let mut resource_limits = None;
    let net_allow_outbound = serde_json::from_value(component.egress_policy)?;
    if let Some(id) = &component.active_version_id {
        resource_limits = Some(
            component_versions::Entity::find_by_id(id)
                .select_only()
                .column(component_versions::Column::ResourceLimits)
                .filter(component_versions::Column::TenantId.eq(tenant))
                .filter(component_versions::Column::ComponentId.eq(&component_id))
                .filter(component_versions::Column::DeletedAt.is_null())
                .into_tuple::<serde_json::Value>()
                .one(&tx)
                .await?
                .ok_or_else(|| FaasError::NotFound("active version".into()))?,
        );
        let live_secret = function_secrets::Entity::find()
            .select_only()
            .column(function_secrets::Column::Id)
            .filter(function_secrets::Column::TenantId.eq(tenant))
            .filter(function_secrets::Column::ComponentId.eq(&component_id))
            .filter(function_secrets::Column::DeletedAt.is_null())
            .filter(
                Expr::col((function_secrets::Entity, function_secrets::Column::Id)).equals((
                    version_secret_bindings::Entity,
                    version_secret_bindings::Column::SecretId,
                )),
            )
            .into_query();
        secrets = version_secret_bindings::Entity::find()
            .select_only()
            .column(version_secret_bindings::Column::Name)
            .column_as(Expr::exists(live_secret), "available")
            .filter(version_secret_bindings::Column::TenantId.eq(tenant))
            .filter(version_secret_bindings::Column::ComponentId.eq(&component_id))
            .filter(version_secret_bindings::Column::VersionId.eq(id))
            .order_by_asc(version_secret_bindings::Column::Name)
            .into_model::<SecretBindingResponse>()
            .all(&tx)
            .await?;
    }
    tx.commit().await?;
    let updated_at = rows.iter().map(|r| r.updated_at).max();
    Ok(Json(FunctionConfigResponse {
        component_id,
        version_id: component.active_version_id,
        resource_limits,
        net_allow_outbound,
        secrets,
        env: rows.into_iter().map(|r| (r.key, r.value)).collect(),
        updated_at,
    }))
}
