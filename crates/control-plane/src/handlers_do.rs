//! M17: Durable Object alarms の内部エンドポイント（worker → CP）。
//!
//! DO storage/lock は worker が自分の Postgres 接続で直接扱う（tenant のみで足りる）が、alarm は
//! **fire 時にどの component を invoke するか**を知る必要があるため component_id を alarm 行に載せる。
//! worker は component_id を持たない（job_token の claim は version_id）ので、set は CP を経由して
//! version_id → component_id を解決してから upsert する。**公開 listener には出さない**（internal のみ）。
//! 認証は job_token の Ed25519 署名で、tenant は claim 由来（worker の主張ではない）。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::collections::HashMap;

use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use faas_shared::FaasError;

#[derive(serde::Deserialize)]
pub struct SetAlarmRequest {
    /// alarm 発火予定（unix ミリ秒, UTC）。
    pub scheduled_at_ms: i64,
}

/// class/id をクエリから取り出す（worker 側が URL エンコードして渡す）。
fn class_id(q: &HashMap<String, String>) -> Result<(String, String), AppError> {
    let class = q
        .get("class")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FaasError::InvalidRequest("missing DO class".into()))?;
    let id = q
        .get("id")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FaasError::InvalidRequest("missing DO id".into()))?;
    Ok((class.clone(), id.clone()))
}

/// PUT /internal/do/alarm?class=&id= — DO インスタンスの alarm を設定（上書き）。
pub async fn set_alarm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    JsonBody(req): JsonBody<SetAlarmRequest>,
) -> Result<impl IntoResponse, AppError> {
    let claims = crate::handlers_r2::claims_from_token(&state, &headers).await?;
    let (class, id) = class_id(&q)?;
    let scheduled_at = chrono::DateTime::from_timestamp_millis(req.scheduled_at_ms)
        .ok_or_else(|| FaasError::InvalidRequest("invalid scheduled_at_ms".into()))?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &claims.tenant_id).await?;
    let component_id =
        db::find_component_id_for_version(&mut *tx, &claims.tenant_id, &claims.version_id)
            .await?
            .ok_or_else(|| FaasError::NotFound(format!("version '{}'", claims.version_id)))?;
    db::upsert_do_alarm(
        &mut *tx,
        &claims.tenant_id,
        &component_id,
        &class,
        &id,
        scheduled_at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /internal/do/alarm?class=&id= — 設定済み alarm 時刻を返す（未設定は 404）。
pub async fn get_alarm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = crate::handlers_r2::tenant_from_token(&state, &headers).await?;
    let (class, id) = class_id(&q)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &tenant).await?;
    let at = db::get_do_alarm(&mut *tx, &tenant, &class, &id).await?;
    tx.commit().await?;

    match at {
        Some(ts) => Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({ "scheduled_at_ms": ts.timestamp_millis() })),
        )
            .into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// DELETE /internal/do/alarm?class=&id= — alarm を削除。
pub async fn delete_alarm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = crate::handlers_r2::tenant_from_token(&state, &headers).await?;
    let (class, id) = class_id(&q)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &tenant).await?;
    db::delete_do_alarm(&mut *tx, &tenant, &class, &id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
