//! in-flight カウンタ reaper + stuck-execution sweeper (M3d, §8)。
//!
//! 同時実行 admission は「invoke で reserve（+1）→ 終端化（subscriber の verified finalize）で
//! DECR（-1）」で回るが、共有カウンタ（Redis）は次の要因で真実からドリフトする:
//! - 終端化での DECR 取りこぼし（subscriber がメッセージを drop、worker クラッシュ等）。
//! - 二重 DECR（再配送 + 既終端でも誤って DECR）— floor で負には居座らないが過小になりうる。
//! - CP/Redis の再起動、TTL 失効。
//!
//! **DB COUNT が唯一の真実**: `SELECT COUNT(*) FROM executions WHERE status IN
//! ('pending','running')`（テナントごと）。reaper は定期的にこの値で共有カウンタを上書き
//! 再同期し、ドリフトを消す。Redis カウンタはあくまで「速い近似」であり、DB COUNT に収束させる。
//!
//! **stuck-execution sweeper（§8 リーク回収）**: ただし DB COUNT 再同期だけでは、終端化されず
//! 永久に pending/running に残る「孤立行」を回収できない —— COUNT がそれを「真実」として数え続け、
//! in-flight スロットが恒久リークするからである。孤立行は次の経路で生じる:
//! - invoke ハンドラが「pending 行 commit → JetStream publish」順で動き、commit 後の publish/ack
//!   失敗で 500 を返すと、ジョブが enqueue されず worker も subscriber も走らない（pending 恒久残留）。
//! - worker 側の取りこぼし（ack-after-publish + max_deliver 再配送でも結果が出ない）で running 残留。
//!
//! M4c (§6.6) の二段救済との関係: worker は `delivered == max_deliver` の最終試行で `.failed`
//! (DLQ) を publish し、CP の DLQ subscriber (`subscriber::run_failed`) が即時に finalize+DECR
//! する。それでも `.failed` の publish 自体が失敗するエッジ（NATS 障害・worker クラッシュ等）に
//! 備えて、本 sweeper が **最終の安全網** として残る。**create_upload は execution 行を INSERT
//! しない**（§6.0; reserved な execution_id のみを返す）ため、本 sweeper の対象には入らない
//! （実際に INSERT されるのは /invoke 経路のみ）。これにより orphan upload は本 sweeper では
//! 拾わず、object storage バケットライフサイクル（TODO 運用側）で物理 GC される。
//!
//! sweeper は各テナントで `created_at < now() - deadline` の非終端行を `failed` に CAS finalize し、
//! 回収後に DB COUNT を再同期する（COUNT が下がりスロットが解放される）。deadline は最悪再配送窓を
//! 十分上回る値にして、正規の遅延結果が再配送中に誤って failed 化されないようにする。
//!
//! テナント列挙: `tenants`（RLS 無し）から全 active テナントを引き、各テナントで GUC を
//! 設定してから sweep / COUNT する（executions は FORCE RLS 下のため GUC 必須）。
//!
//! fail-mode: reaper は **best-effort**。Redis 到達不能（[`StoreError::is_unavailable`]）や
//! 一時的 DB エラーで落とさず、次の周期で再試行する。長期に走らせる安全網であり、単発失敗で
//! プロセスを巻き込まない。

use std::time::Duration;

use crate::state::AppState;
use crate::store::StoreError;

/// reaper ループ。`main` から `tokio::spawn` される。
///
/// `interval_secs` 周期で全 active テナントの孤立行を sweep し、カウンタを DB COUNT へ再同期する。
/// `inflight_ttl_secs` は再同期時にカウンタへ貼り直す TTL（孤立カウンタの保険）。
/// `stuck_deadline_secs` は孤立 pending/running 行を failed 化する deadline（0 で sweep 無効）。
pub async fn run(
    state: AppState,
    interval_secs: u64,
    inflight_ttl_secs: u64,
    stuck_deadline_secs: u64,
) {
    // 0 は「無効」を意味させず、最低 1 秒に丸めて暴走 tight-loop を防ぐ。
    let period = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    // 起動直後に 1 回走らせる（tick 0 は即座に返る）。
    tracing::info!(
        interval_secs = period.as_secs(),
        stuck_deadline_secs,
        "in-flight reaper + stuck-execution sweeper started"
    );

    loop {
        ticker.tick().await;
        if let Err(e) = reconcile_once(&state, inflight_ttl_secs, stuck_deadline_secs).await {
            // best-effort: 周期内の失敗は記録のみ。次の周期で再試行する。
            tracing::warn!(error = %e, "in-flight reaper pass failed; will retry next interval");
        }
    }
}

/// 1 周期分の sweep + 再同期。全 active テナントを処理する。
///
/// テナント列挙の失敗（DB 障害）はパス全体を失敗扱いにする（次周期で再試行）。個々の
/// テナントの sweep / COUNT / resync 失敗はそのテナントだけスキップし、他テナントの処理は続ける
/// （1 テナントの一時障害が全体を止めない）。
async fn reconcile_once(
    state: &AppState,
    inflight_ttl_secs: u64,
    stuck_deadline_secs: u64,
) -> anyhow::Result<()> {
    // tenants は RLS 無し → GUC 不要でプールから直接列挙できる。
    let tenants = crate::db::list_active_tenant_ids(state.pool()).await?;

    let mut resynced = 0usize;
    for tenant in &tenants {
        match reconcile_tenant(state, tenant, inflight_ttl_secs, stuck_deadline_secs).await {
            Ok(()) => resynced += 1,
            Err(e) => {
                // Redis 到達不能はこの周期では諦める（fail-open: admission は別途素通し）。
                // DB エラーは当該テナントのみスキップ。いずれも次周期で再試行。
                tracing::warn!(tenant = %tenant, error = %e, "reaper: failed to reconcile tenant");
            }
        }
    }

    // M4a (§3.8): 直近 1 周期で扱ったテナント数を gauge にスナップする（再同期成否を問わず
    // 列挙できたテナント数）。reaper の停滞を可視化する 1 シグナルとして使う。
    state
        .metrics()
        .reaper_tenants_last
        .set(tenants.len() as i64);

    tracing::debug!(
        tenants = tenants.len(),
        resynced,
        "in-flight reaper pass complete"
    );
    Ok(())
}

/// 1 テナント分の sweep + 再同期: GUC を設定して (1) 孤立行を failed 化し DECR、(2) DB COUNT で
/// 共有カウンタを上書きする。
async fn reconcile_tenant(
    state: &AppState,
    tenant: &str,
    inflight_ttl_secs: u64,
    stuck_deadline_secs: u64,
) -> anyhow::Result<()> {
    // executions は FORCE RLS 下 → sweep / COUNT は GUC を設定した同一 tx で実行する。
    let mut tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&mut tx, tenant).await?;

    // (1) stuck-execution sweep（§8 リーク回収）。deadline 0 ならスキップ。
    //     deadline を過ぎても終端化されない pending/running 行を failed に倒し、回収数を得る。
    let swept = if stuck_deadline_secs > 0 {
        crate::db::finalize_stuck_executions(&mut *tx, tenant, stuck_deadline_secs as i64).await?
    } else {
        Vec::new()
    };

    // (2) DB COUNT（真実）を引く。sweep 済み行は除外されているので、孤立スロットは COUNT から消える。
    let count = crate::db::count_inflight_executions(&mut *tx, tenant).await?;
    tx.commit().await?;

    if !swept.is_empty() {
        // M4a (§3.8): sweeper 回収件数と、終端化された executions（status=failed）を観測する。
        // ここで inc される `executions_total{status="failed"}` は subscriber の正規 finalize と
        // 同名カウンタに合流するため、合算で「失敗合計」が取れる（sweeper か worker 起因かは
        // reaper_swept_total で分けられる）。
        state
            .metrics()
            .reaper_swept_total
            .inc_by(swept.len() as u64);
        state
            .metrics()
            .executions_total
            .with_label_values(&["failed"])
            .inc_by(swept.len() as u64);

        tracing::warn!(
            tenant = %tenant,
            swept = swept.len(),
            deadline_secs = stuck_deadline_secs,
            "stuck-execution sweeper finalized orphaned pending/running rows to 'failed' (reclaiming in-flight slots)"
        );
        // 回収した各行ぶん DECR する（subscriber の終端 DECR と同じ契約）。最終的には下の resync が
        // DB COUNT（真実）で上書きするため、ここの DECR 失敗は無害（best-effort）。
        for _ in &swept {
            if let Err(e) = state.store().release_inflight(tenant).await {
                tracing::warn!(tenant = %tenant, error = %e, "sweeper DECR failed; resync will correct");
                break;
            }
        }
    }

    // (3) DB COUNT（真実）で共有カウンタを上書きする。Redis 到達不能はそのテナントだけ諦める。
    state
        .store()
        .resync_inflight(tenant, count, inflight_ttl_secs)
        .await
        .map_err(|e: StoreError| anyhow::anyhow!("resync_inflight: {e}"))?;
    Ok(())
}
