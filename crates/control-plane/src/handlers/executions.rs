//! Executions management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_database::prelude::*;
use hibana_shared::FaasError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
pub struct ExecutionsQuery {
    #[serde(default)]
    pub errors_only: bool,
    pub before: Option<String>,
}

/// A bounded operational view. Request/response bodies and storage references are excluded.
#[derive(Debug, Serialize, FromQueryResult)]
pub struct ExecutionSummary {
    pub execution_id: String,
    pub version_id: String,
    pub status: String,
    #[serde(serialize_with = "serialize_http_status")]
    pub http_status: Option<Value>,
    pub error: Option<Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub wall_time_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Value>,
}

fn serialize_http_status<S: serde::Serializer>(
    value: &Option<Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    db::http_status_code(value.as_ref()).serialize(serializer)
}

pub async fn list_executions(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    Query(query): Query<ExecutionsQuery>,
) -> Result<impl IntoResponse, AppError> {
    execution_page(state, principal, component_id, query, false).await
}

pub async fn list_logs(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    Query(query): Query<ExecutionsQuery>,
) -> Result<impl IntoResponse, AppError> {
    execution_page(state, principal, component_id, query, true).await
}

async fn execution_page(
    state: AppState,
    principal: Principal,
    component_id: String,
    query: ExecutionsQuery,
    include_logs: bool,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let mut select = executions::Entity::find()
        .select_only()
        .column_as(executions::Column::Id, "execution_id")
        .column_as(db::http_status_expression(), "http_status")
        .column_as(
            if include_logs {
                db::application_logs_expression()
            } else {
                Expr::val(None::<Value>)
            },
            "logs",
        )
        .columns([
            executions::Column::VersionId,
            executions::Column::Status,
            executions::Column::Error,
            executions::Column::CreatedAt,
            executions::Column::WallTimeMs,
        ])
        .filter(executions::Column::TenantId.eq(tenant))
        .filter(executions::Column::ComponentId.eq(&component_id))
        .filter(
            executions::Column::CreatedAt.gte(chrono::Utc::now() - chrono::Duration::hours(24)),
        );
    if query.errors_only {
        select = select.filter(db::execution_errors_condition());
    }
    if let Some(cursor) = query.before {
        let invalid = || FaasError::InvalidRequest("invalid execution cursor".into());
        let (created, id) = cursor.split_once(',').ok_or_else(invalid)?;
        let created = chrono::DateTime::parse_from_rfc3339(created)
            .map_err(|_| invalid())?
            .with_timezone(&chrono::Utc);
        if id.is_empty() || id.len() > 128 {
            return Err(invalid().into());
        }
        select = select.filter(
            Condition::any()
                .add(executions::Column::CreatedAt.lt(created))
                .add(
                    Condition::all()
                        .add(executions::Column::CreatedAt.eq(created))
                        .add(executions::Column::Id.lt(id)),
                ),
        );
    }
    let mut items = select
        .order_by_desc(executions::Column::CreatedAt)
        .order_by_desc(executions::Column::Id)
        .limit(21)
        .into_model::<ExecutionSummary>()
        .all(&tx)
        .await?;
    tx.commit().await?;
    let more = items.len() > 20;
    items.truncate(20);
    let next_cursor = if more {
        items
            .last()
            .map(|e| format!("{},{}", e.created_at.to_rfc3339(), e.execution_id))
    } else {
        None
    };
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"items": items, "next_cursor": next_cursor})),
    ))
}

/// Operational details only, including while dispatch input still exists.
#[derive(Debug, Serialize)]
pub struct ExecutionResponse {
    pub execution_id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub http_status: Option<u16>,
    pub error: Option<Value>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub logs: Option<Value>,
}

pub async fn get_execution(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    // GET も RLS 下では app.tenant_id を SELECT 実行接続にセットする必要がある
    // （GUC 未設定の bare 接続は fail-closed で ERROR）。
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let row = db::get_execution(&tx, tenant, &id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("execution '{id}'")))?;
    tx.commit().await?;

    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(ExecutionResponse {
            execution_id: row.id,
            tenant_id: row.tenant_id,
            component_id: row.component_id,
            version_id: row.version_id,
            status: row.status,
            http_status: db::http_status_code(row.http_status.as_ref()),
            error: row.error,
            logs: row.logs,
            created_at: row.created_at.to_rfc3339(),
            started_at: row.started_at.map(|t| t.to_rfc3339()),
            finished_at: row.finished_at.map(|t| t.to_rfc3339()),
        }),
    ))
}
