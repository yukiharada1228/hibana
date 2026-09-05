//! M15: Queues バインディング。
//!
//! producer `env.QUEUE.send()` は worker 経由で内部エンドポイント `/internal/queue/send` に届き、
//! ここで **consumer コンポーネントの invoke をキュー envelope で enqueue** する（メッセージ＝invoke）。
//! 再試行/backoff/DLQ は既存の JetStream パイプラインがそのまま担う。consumer は shim が
//! `/__hibana/queue` を検知して `app.queue(batch)` へ dispatch する。
//!
//! consumer 登録（どの queue を どの component が consume するか）は deploy 時に `PUT
//! /components/{id}/queue-consumers` で行う（Deploy スコープ）。

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::enqueue::{self, EnqueueRequest};
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::{auth::Principal, db};
use faas_shared::FaasError;

#[derive(Debug, Deserialize)]
pub struct RegisterConsumersRequest {
    /// この component が consume する queue 名の一覧。
    pub queues: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterConsumersResponse {
    pub component_id: String,
    pub queues: Vec<String>,
}

/// PUT /components/{id}/queue-consumers — この component を指定 queue の consumer に登録（Deploy）。
pub async fn register_consumers(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    JsonBody(req): JsonBody<RegisterConsumersRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;
    for q in &req.queues {
        db::upsert_queue_consumer(&mut *tx, tenant, q, &component_id).await?;
    }
    tx.commit().await?;
    Ok(axum::Json(RegisterConsumersResponse {
        component_id,
        queues: req.queues,
    }))
}

#[derive(Debug, Deserialize)]
pub struct QueueSendRequest {
    pub queue: String,
    /// 1 回の send/sendBatch のメッセージ本体（任意 JSON）。
    pub messages: Vec<serde_json::Value>,
}

/// POST /internal/queue/send — producer からのメッセージを consumer の invoke として enqueue する。
/// internal listener のみ。認証は job_token（テナントは claim 由来）。
pub async fn queue_send(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<QueueSendRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = crate::handlers_r2::tenant_from_token(&state, &headers).await?;
    if req.messages.is_empty() {
        return Ok((StatusCode::NO_CONTENT).into_response());
    }

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &tenant).await?;

    let comp_id = db::find_queue_consumer(&mut *tx, &tenant, &req.queue)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("no consumer registered for queue '{}'", req.queue))
        })?;
    let comp = db::find_component_by_id(&mut *tx, &tenant, &comp_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("consumer component '{comp_id}'")))?;

    // canary 解決（cron/HTTP と同一経路）。routing key はメッセージ毎にユニーク（dedup しない）。
    let routing_key = faas_shared::new_execution_id();
    let routed = enqueue::resolve_version_for_enqueue(&mut tx, &tenant, &comp.name, &routing_key)
        .await?
        .ok_or_else(|| {
            FaasError::Conflict(format!(
                "consumer '{}' for queue '{}' has no active version",
                comp.name, req.queue
            ))
        })?;
    let active = &routed.selected;

    let wasm_url = state
        .storage()
        .presign_get(&active.storage_uri, state.presign_ttl())
        .await?;
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(active.max_wall_time_ms);

    // consumer への配送は HTTP エンベロープ（POST /__hibana/queue）。shim がこの path を見て
    // app.queue(batch) へ dispatch する。body に queue 名とメッセージ群を載せる。
    let batch_body = serde_json::json!({ "queue": req.queue, "messages": req.messages }).to_string();
    let envelope = serde_json::json!({
        "method": "POST",
        "path": "/__hibana/queue",
        "headers": { "content-type": "application/json" },
        "body": batch_body,
        "bodyBase64": false,
    });

    let execution_id = faas_shared::new_execution_id();
    let outcome = enqueue::enqueue_execution(
        &state,
        tx,
        EnqueueRequest {
            tenant: &tenant,
            component_name: &comp.name,
            component_id: &routed.component_id,
            version_id: &active.version_id,
            version: &active.version,
            wasm_sha256: &active.wasm_sha256,
            wasm_url,
            input_url: None,
            input: &envelope,
            input_ref: None,
            execution_id: execution_id.clone(),
            idempotency_key: None,
            request_hash: None,
            iat,
            exp,
            reply_to: None,
            origin: "queue",
            chain_depth: 0,
            routing_reason: routed.reason,
        },
    )
    .await;

    match outcome {
        Ok(_) => Ok((
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "execution_id": execution_id })),
        )
            .into_response()),
        Err(enqueue::EnqueueError::PublishBackpressure) => {
            Ok(crate::admission::RateLimited::rate(1).into_response())
        }
        Err(enqueue::EnqueueError::Other(e)) => Err(e),
    }
}
