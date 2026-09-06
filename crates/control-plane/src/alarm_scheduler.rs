//! Durable Object alarm スケジューラ (M17, §11)。
//!
//! cron スケジューラ（`scheduler.rs`）と同型。`ALARM_POLL_INTERVAL_SECS` 周期で「いま due な DO alarm」
//! を全テナント横断で引き、各 alarm を **HTTP invoke と同一の enqueue 正規パス**へ合流させて
//! `alarm()` ハンドラを起動する（POST /__hibana/alarm）。これにより provenance（job_token 署名）・
//! 計量（単一 finalize）・テナント分離（FORCE RLS）・再配送（JetStream backoff/DLQ）が HTTP と同じ
//! 不変条件で担保される。
//!
//! ## single-flight（multi-instance 二重発火の防止）
//! due 観測 → 行を `FOR UPDATE SKIP LOCKED` で掴む → **行を削除（one-shot）** してから
//! `enqueue_execution(origin="alarm")` を **同一 tx** で呼び commit する。二重発火は行ロックで吸収する
//! （最初に掴んだ CP だけが削除＋enqueue へ進む）。alarm() 内で再度 setAlarm すれば周期化できる。
//!
//! ## admission
//! cron fire と同じく rate-limit / in-flight reserve は **しない**（定時起動が 429 で落ちない方針）。

use std::time::Duration;

use crate::enqueue::{self, EnqueueError, EnqueueOutcome, EnqueueRequest};
use crate::state::AppState;

/// DO alarm スケジューラループ。`main` から `tokio::spawn` される。
pub async fn run(state: AppState, poll_interval_secs: u64) {
    let period = Duration::from_secs(poll_interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(
        poll_interval_secs = period.as_secs(),
        "DO alarm scheduler started"
    );

    loop {
        ticker.tick().await;
        if let Err(e) = poll_once(&state).await {
            tracing::warn!(error = %e, "alarm scheduler pass failed; will retry next interval");
        }
    }
}

/// 1 周期分の due スキャン + fire。
async fn poll_once(state: &AppState) -> anyhow::Result<()> {
    let due = crate::db::do_due_alarms(state.pool()).await?;
    let mut fired = 0usize;
    for (tenant, class, id) in &due {
        match fire_due_alarm(state, tenant, class, id).await {
            Ok(true) => fired += 1,
            Ok(false) => { /* 他 CP 先取り or 掴んだ時点で due でない → skip */ }
            Err(e) => {
                tracing::warn!(tenant = %tenant, do_class = %class, do_id = %id, error = %e, "alarm fire failed; will retry next interval");
            }
        }
    }
    if fired > 0 {
        tracing::debug!(due = due.len(), fired, "alarm scheduler pass complete");
    }
    Ok(())
}

/// 1 つの due alarm を fire する。enqueue まで進んだら `Ok(true)`、ロック競合/非 due なら `Ok(false)`。
async fn fire_due_alarm(
    state: &AppState,
    tenant: &str,
    class: &str,
    id: &str,
) -> anyhow::Result<bool> {
    let mut tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&mut tx, tenant).await?;

    // single-flight: 行を FOR UPDATE SKIP LOCKED で掴む。掴めない/非 due なら skip。
    let Some(alarm) = crate::db::lock_due_do_alarm(&mut *tx, tenant, class, id).await? else {
        return Ok(false);
    };

    let component = crate::db::find_component_by_id(&mut *tx, tenant, &alarm.component_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "alarm references missing component '{}'",
                alarm.component_id
            )
        })?;

    // canary 解決（HTTP/cron と同一経路）。routing key は fire 毎にユニーク。
    let routing_key = faas_shared::new_execution_id();
    let routed =
        enqueue::resolve_version_for_enqueue(&mut tx, tenant, &component.name, &routing_key)
            .await?;
    let Some(routed) = routed else {
        // active version が無いなら fire できない。one-shot なので行は削除して skip（tight-loop 回避）。
        crate::db::delete_do_alarm(&mut *tx, tenant, class, id).await?;
        tx.commit().await?;
        tracing::warn!(
            tenant = %tenant, do_class = %class, do_id = %id, component = %component.name,
            "alarm's component has no active version; dropped alarm"
        );
        return Ok(false);
    };
    let active = &routed.selected;

    // one-shot: 掴んでいる間に行を削除する（次 poll で due に当たらない）。enqueue が同一 tx を commit する。
    crate::db::delete_do_alarm(&mut *tx, tenant, class, id).await?;

    let wasm_url = state
        .storage()
        .presign_get(&active.storage_uri, state.presign_ttl())
        .await?;
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(active.max_wall_time_ms);

    // 配送は HTTP エンベロープ（POST /__hibana/alarm）。shim が path を見て DO の alarm() へ dispatch。
    let body = serde_json::json!({ "class": class, "id": id }).to_string();
    let envelope = serde_json::json!({
        "method": "POST",
        "path": "/__hibana/alarm",
        "headers": { "content-type": "application/json" },
        "body": body,
        "bodyBase64": false,
    });

    let execution_id = faas_shared::new_execution_id();
    let outcome = enqueue::enqueue_execution(
        state,
        tx,
        EnqueueRequest {
            tenant,
            component_name: &component.name,
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
            origin: "alarm",
            chain_depth: 0,
            routing_reason: routed.reason,
        },
    )
    .await;

    match outcome {
        Ok(EnqueueOutcome::Enqueued { execution_id }) => {
            tracing::info!(tenant = %tenant, do_class = %class, do_id = %id, %execution_id, "DO alarm fired (enqueued)");
            Ok(true)
        }
        Ok(EnqueueOutcome::IdempotentHit { execution_id, .. }) => {
            tracing::debug!(tenant = %tenant, do_class = %class, do_id = %id, existing_execution_id = %execution_id, "DO alarm already fired (idempotent)");
            Ok(true)
        }
        Err(EnqueueError::PublishBackpressure) => {
            tracing::warn!(tenant = %tenant, do_class = %class, do_id = %id, "alarm fire publish backpressure; row already deleted, will not refire");
            Ok(true)
        }
        Err(EnqueueError::Other(e)) => Err(anyhow::anyhow!("alarm enqueue failed: {}", e.0)),
    }
}
