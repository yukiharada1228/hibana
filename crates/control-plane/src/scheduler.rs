//! Cron スケジューラ (M6b, §11 / §15)。
//!
//! `CRON_POLL_INTERVAL_SECS` 周期で「いま due な Cron ジョブ」を全テナント横断で引き、各ジョブを
//! HTTP invoke と **同一の enqueue 正規パス**（`enqueue::enqueue_execution`）へ合流させて起動する。
//! これにより Cron 起点でも provenance（job_token 署名）・冪等性（3 層）・計量（M5 の単一 finalize +
//! usage_rollups）・テナント分離（FORCE RLS）が HTTP と同じ不変条件で担保される（§4 不変条件）。
//!
//! ## single-flight（multi-instance 二重発火の防止）
//! due 観測 → 行を `FOR UPDATE SKIP LOCKED` で掴む → `scheduled_slot=floor(next_fire_at)` から
//! `cron_idempotency_key(job_id, slot)` を作り、`next_fire_at` を次回 occurrence へ前進 UPDATE して
//! から `enqueue_execution(origin="cron", idempotency_key=Some(idem_key))` を **同一 tx** で呼び commit
//! する。二重発火は (a) `FOR UPDATE SKIP LOCKED` の行ロック（最初に掴んだ CP だけが進む）と
//! (b) `executions(tenant_id, idempotency_key)` UNIQUE（同一 slot→同一キー）の **二重防御**で吸収する。
//!
//! ## admission
//! Cron fire は rate-limit / in-flight reserve を **しない**（定時バッチが 429 で落ちるのは望ましく
//! ない方針, 設計 §5.4）。計量は依然 subscriber の単一 finalize 経由なので二重計上しない。
//!
//! 全テナント巡回（due スキャン）は RLS 下で GUC 無しに走らせられないため、`cron_due_tenant_jobs()`
//! （SECURITY DEFINER）で対象 (tenant_id, job_id) を引いてから、各テナントごとに `set_tenant_guc`
//! した tx で fire 本処理（lock / advance / enqueue）を RLS 下で行う（reaper の巡回と同型）。

use std::time::Duration;

use crate::cron::{self, scheduled_slot_unix};
use crate::enqueue::{self, EnqueueError, EnqueueOutcome, EnqueueRequest};
use crate::state::AppState;

/// Cron スケジューラループ。`main` から `tokio::spawn` される。
///
/// `poll_interval_secs` 周期で 1 回 due スキャン + fire する。0 は無効を意味させず最低 1 秒に丸めて
/// tight-loop 暴走を防ぐ（reaper と同じ運用）。
pub async fn run(state: AppState, poll_interval_secs: u64) {
    let period = Duration::from_secs(poll_interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(
        poll_interval_secs = period.as_secs(),
        "cron scheduler started"
    );

    loop {
        ticker.tick().await;
        if let Err(e) = poll_once(&state).await {
            // best-effort: 周期内の失敗は記録のみ。次の周期で再試行する。
            tracing::warn!(error = %e, "cron scheduler pass failed; will retry next interval");
        }
    }
}

/// 1 周期分の due スキャン + fire。due な全 (tenant, job) を処理する。
///
/// due 列挙（SECURITY DEFINER 関数）の失敗はパス全体を失敗扱い（次周期で再試行）。個々のジョブの
/// fire 失敗（ロック競合での skip を除く実エラー）はそのジョブだけ記録してスキップし、他ジョブの処理は
/// 続ける（1 ジョブの一時障害が全体を止めない）。
async fn poll_once(state: &AppState) -> anyhow::Result<()> {
    // due スキャンは cron_due_tenant_jobs()（SECURITY DEFINER）で GUC 無し巡回参照する。
    let due = crate::db::cron_due_tenant_jobs(state.pool()).await?;
    let mut fired = 0usize;
    for (tenant, job_id) in &due {
        match fire_due_job(state, tenant, job_id).await {
            Ok(true) => fired += 1,
            Ok(false) => { /* 他 CP が先取り（SKIP LOCKED）or 掴んだ時点で due でない → skip */
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, cron_job_id = %job_id, error = %e, "cron fire failed; will retry next interval");
            }
        }
    }
    if fired > 0 {
        tracing::debug!(due = due.len(), fired, "cron scheduler pass complete");
    }
    Ok(())
}

/// 1 つの due ジョブを fire する。実際に enqueue（または冪等吸収）まで進んだら `Ok(true)`、
/// ロック競合 / 掴んだ時点で due でない場合は `Ok(false)`（skip）。
///
/// presign やトークン exp 計算は HTTP invoke と同じ手順で行い、`enqueue_execution(origin="cron")`
/// に合流させる。tx（lock / advance）は `enqueue_execution` が INSERT 後に commit するため、
/// 「lock → advance → pending INSERT → commit」が原子的に確定する（前進と起動が同一 tx）。
async fn fire_due_job(state: &AppState, tenant: &str, job_id: &str) -> anyhow::Result<bool> {
    let mut tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&mut tx, tenant).await?;

    // single-flight: 行を FOR UPDATE SKIP LOCKED で掴む。掴めない（他 CP 先取り）/ 掴んだ時点で
    // もう due でない（他 CP が前進済み）なら None → skip。
    let Some(job) = crate::db::lock_due_cron_job(&mut *tx, tenant, job_id).await? else {
        return Ok(false);
    };

    // active component / version を解決する（未デプロイ・active 無しは fire をスキップして前進だけ
    // させたい —— が、component 名が引けないと presign できない。component_id から名前を引く）。
    let component = crate::db::find_component_by_id(&mut *tx, tenant, &job.component_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cron job references missing component '{}'",
                job.component_id
            )
        })?;
    let active = crate::db::active_version_storage(&mut *tx, tenant, &component.name).await?;
    let Some(active) = active else {
        // active version が無いジョブは起動できない。掴んでいるロック tx をそのまま渡して前進だけ
        // させ、次回 due で再試行する（毎 poll で同じジョブを掴み続ける tight-loop を避ける）。
        advance_only(tx, tenant, &job).await?;
        tracing::warn!(
            tenant = %tenant,
            cron_job_id = %job.id,
            component = %component.name,
            "cron job's component has no active version; advanced next_fire_at and skipped this slot"
        );
        return Ok(false);
    };

    // 今回 fire のスロット（分境界 unix 秒）と安定冪等キー。
    let slot = scheduled_slot_unix(job.next_fire_at);
    let idem_key = faas_shared::cron_idempotency_key(&job.id, slot);

    // 次回 occurrence を計算し、next_fire_at を前進させてからコミットする（掴んでいる間に重複 due を
    // 消す）。cron 式は登録時に検証済みだが、防御的に再パースして次回を求める。
    let next = cron::CronSchedule::parse(&job.schedule)
        .ok()
        .and_then(|s| s.next_after(job.next_fire_at))
        // 万一パース不能/無発火なら poll 間隔より十分後ろへ退避させ tight-loop を避ける。
        .unwrap_or_else(|| job.next_fire_at + chrono::Duration::hours(1));
    crate::db::advance_cron_next_fire(&mut *tx, tenant, &job.id, next, slot).await?;

    // wasm 本体への短命 presigned GET URL を発行する（HTTP invoke と同条件, §3.4）。
    let wasm_url = state
        .storage()
        .presign_get(&active.storage_uri, state.presign_ttl())
        .await?;

    // job_token の iat/exp を HTTP と同じ計算で確定する（provenance §3.3）。
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(active.max_wall_time_ms);

    // HTTP invoke と同一の enqueue 正規パスへ合流する（origin="cron", reply_to=None）。tx（lock /
    // advance 済み）を渡し、ヘルパが pending INSERT → commit → 署名 → publish する。
    let request_hash = cron_request_hash(&component.name, &job.input);
    let outcome = enqueue::enqueue_execution(
        state,
        tx,
        EnqueueRequest {
            tenant,
            component_name: &component.name,
            component_id: &component.id,
            version_id: &active.version_id,
            version: &active.version,
            wasm_sha256: &active.wasm_sha256,
            wasm_url,
            input_url: None,
            input: &job.input,
            input_ref: None,
            execution_id: faas_shared::new_execution_id(),
            idempotency_key: Some(&idem_key),
            request_hash: Some(&request_hash),
            iat,
            exp,
            reply_to: None,
            origin: "cron",
            chain_depth: 0,
        },
    )
    .await;

    match outcome {
        Ok(EnqueueOutcome::Enqueued { execution_id }) => {
            tracing::info!(
                tenant = %tenant,
                cron_job_id = %job.id,
                %execution_id,
                slot,
                "cron job fired (enqueued)"
            );
            Ok(true)
        }
        // 冪等ヒット: 同一 slot を別 CP（または前回 poll）が既に enqueue 済み。二重発火は吸収され、
        // この CP は何もしない（DB ロック + UNIQUE の二重防御が効いた正常系）。
        Ok(EnqueueOutcome::IdempotentHit { execution_id, .. }) => {
            tracing::debug!(
                tenant = %tenant,
                cron_job_id = %job.id,
                existing_execution_id = %execution_id,
                slot,
                "cron slot already fired (idempotent); skipping double-fire"
            );
            Ok(true)
        }
        // publish 失敗（バックプレッシャ）: pending 行は commit 済み（reaper の stuck sweeper が
        // deadline で failed に倒す）。Cron は 429 を返す先がないので記録のみで次周期に委ねる（設計 §M6-0
        // follow-up: non-HTTP は 429 へ写像しない）。next_fire_at は前進済みなので二重 fire はしない。
        Err(EnqueueError::PublishBackpressure) => {
            tracing::warn!(
                tenant = %tenant,
                cron_job_id = %job.id,
                slot,
                "cron fire publish backpressure; pending row left for sweeper, advanced next_fire_at"
            );
            Ok(true)
        }
        Err(EnqueueError::Other(e)) => Err(anyhow::anyhow!("cron enqueue failed: {}", e.0)),
    }
}

/// active version が無いジョブの next_fire_at だけを前進させる（fire はスキップ）。
///
/// 専用の短い tx で行う（呼び出し側の lock tx は enqueue へ渡さず drop される文脈のため、ここで
/// 独立 tx を開いて前進だけ確定する）。tight-loop（毎 poll 同一ジョブを掴み続ける）を避けるのが目的。
async fn advance_only(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    job: &crate::db::CronJobRow,
) -> anyhow::Result<()> {
    let slot = scheduled_slot_unix(job.next_fire_at);
    let next = cron::CronSchedule::parse(&job.schedule)
        .ok()
        .and_then(|s| s.next_after(job.next_fire_at))
        .unwrap_or_else(|| job.next_fire_at + chrono::Duration::hours(1));
    // 呼び出し側が掴んだ行ロック（FOR UPDATE SKIP LOCKED）を保持したまま前進させ、commit でロックを
    // 解放する。別 tx を開き直さないことで、active version 無しジョブの前進も single-flight 規律を保つ
    // （2 つの CP が同一 next_fire_at を競って二重 UPDATE する瞬間を作らない）。tx は GUC 設定済み。
    crate::db::advance_cron_next_fire(&mut *tx, tenant, &job.id, next, slot).await?;
    tx.commit().await?;
    Ok(())
}

/// Cron fire の冪等 body hash（HTTP invoke の `invoke_request_hash` と同趣旨）。
///
/// 同一 (component, input) は同一 hash になるよう canonical JSON の sha256 hex を返す。Cron は同一
/// slot→同一 idempotency_key で二重発火を吸収するが、冪等列の body hash も整合的に保存しておく
/// （IdempotentHit 時の body 一致観測・将来の差分検出に使う）。
fn cron_request_hash(component: &str, input: &serde_json::Value) -> String {
    crate::handlers::invoke_request_hash_for(component, input, None)
}
