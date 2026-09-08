//! Executions management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::FaasError;
use serde::Serialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ExecutionResponse {
    pub execution_id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<Value>,
    /// M3d (§6.4): 大入力の退避参照（インラインなら null）。
    pub input_ref: Option<String>,
    /// M3d (§6.4): 大出力の退避参照（インラインなら null）。
    pub output_ref: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

pub async fn get_execution(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    // GET も RLS 下では app.tenant_id を SELECT 実行接続にセットする必要がある
    // （GUC 未設定の bare 接続は fail-closed で ERROR）。
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = db::get_execution(&mut *tx, tenant, &id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("execution '{id}'")))?;
    tx.commit().await?;

    Ok(Json(ExecutionResponse {
        execution_id: row.id,
        tenant_id: row.tenant_id,
        component_id: row.component_id,
        version_id: row.version_id,
        status: row.status,
        input: row.input,
        output: row.output,
        error: row.error,
        input_ref: row.input_ref,
        output_ref: row.output_ref,
        created_at: row.created_at.to_rfc3339(),
        started_at: row.started_at.map(|t| t.to_rfc3339()),
        finished_at: row.finished_at.map(|t| t.to_rfc3339()),
    }))
}
