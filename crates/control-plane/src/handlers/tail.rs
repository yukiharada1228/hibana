//! Live execution notifications. Each poll rechecks Read scope and tenant RLS.
use super::executions::ExecutionSummary;
use crate::{auth::Principal, db, error::AppError, state::AppState, store::tail};
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use hibana_database::prelude::*;
use hibana_shared::FaasError;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailQuery {
    pub cursor: Option<String>,
    pub status: Option<String>,
    pub search: Option<String>,
    pub version_id: Option<String>,
}

impl TailQuery {
    fn validate(&self) -> Result<(), FaasError> {
        if self
            .cursor
            .as_deref()
            .is_some_and(|v| !tail::valid_cursor(v))
        {
            return Err(FaasError::InvalidRequest("invalid tail cursor".into()));
        }
        if self
            .status
            .as_deref()
            .is_some_and(|v| !matches!(v, "ok" | "error" | "canceled"))
        {
            return Err(FaasError::InvalidRequest(
                "tail status must be ok, error or canceled".into(),
            ));
        }
        if self
            .search
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 1024)
            || self
                .version_id
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 128)
        {
            return Err(FaasError::InvalidRequest("invalid tail filter".into()));
        }
        Ok(())
    }
    fn matches(&self, item: &ExecutionSummary) -> bool {
        let outcome = if item.status == "succeeded" {
            "ok"
        } else {
            "error"
        };
        self.status.as_deref().is_none_or(|v| v == outcome)
            && self
                .version_id
                .as_ref()
                .is_none_or(|v| *v == item.version_id)
            && self.search.as_ref().is_none_or(|needle| {
                ["stdout", "stderr"].iter().any(|stream| {
                    item.logs
                        .as_ref()
                        .and_then(|v| v.get(stream))
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.contains(needle))
                })
            })
    }
}

pub async fn poll(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    Query(query): Query<TailQuery>,
) -> Result<impl IntoResponse, AppError> {
    query.validate()?;
    let tenant = &principal.tenant_id;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    // Redis reconnection must not occupy a database connection. Quiet polls
    // finish here; only a page containing notifications opens another transaction.
    tx.commit().await?;
    let page = state
        .store()
        .read_tail(tenant, &component_id, query.cursor.as_deref())
        .await
        .map_err(|_| FaasError::Unavailable)?;
    let rows = if page.execution_ids.is_empty() {
        Vec::new()
    } else {
        let tx = state.pool().begin().await?;
        db::set_tenant_guc(&tx, tenant).await?;
        db::find_component_by_id(&tx, tenant, &component_id)
            .await?
            .ok_or_else(|| FaasError::NotFound("component".into()))?;
        let rows = db::execution_summary_query(tenant, &component_id, true)
            .filter(executions::Column::Id.is_in(page.execution_ids.iter().cloned()))
            .filter(executions::Column::Status.is_in(["succeeded", "failed", "timeout"]))
            .into_model::<ExecutionSummary>()
            .all(&tx)
            .await?;
        tx.commit().await?;
        rows
    };
    // Notification order is independent of execution start time, duration and CP replica.
    let mut rows: std::collections::HashMap<_, _> = rows
        .into_iter()
        .map(|row| (row.execution_id.clone(), row))
        .collect();
    let items: Vec<_> = page
        .execution_ids
        .iter()
        .filter_map(|id| rows.remove(id))
        .filter(|item| query.matches(item))
        .collect();
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "items": items, "cursor": page.cursor, "has_more": page.has_more, "lagged": page.lagged,
        })),
    ))
}
