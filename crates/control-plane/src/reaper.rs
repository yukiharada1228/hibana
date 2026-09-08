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
                // Redis 到達不能はこの周期では諦める。admission は fail-closed で拒否する。
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

    for row in &swept {
        crate::db::upsert_usage_rollup(
            &mut *tx,
            tenant,
            &row.component_id,
            row.period_start,
            hibana_shared::ExecutionStatus::Failed,
            &hibana_shared::UsageMetrics::default(),
        )
        .await?;
    }

    // (2) DB COUNT（真実）を引く。sweep 済み行は除外されているので、孤立スロットは COUNT から消える。
    let count = crate::db::count_inflight_executions(&mut *tx, tenant).await?;
    tx.commit().await?;

    if !swept.is_empty() {
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
            swept_ids = ?swept.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            deadline_secs = stuck_deadline_secs,
            "stuck-execution sweeper finalized orphaned pending/running rows to 'failed' (reclaiming in-flight slots)"
        );
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

// ---------------------------------------------------------------------------
// M7c-4: KEK ローテーションの進捗 gauge を更新する背景ジョブ (§4.7.2)
// ---------------------------------------------------------------------------

/// `faas_secret_versions_by_kid` を定期更新する（`scheduler::run` / `reaper::run` と同型のループ）。
///
/// 全テナント横断の集計なので **SECURITY DEFINER 関数の `_all` 版**を使い、結果は
/// **Prometheus gauge にしか出さない**（HTTP 応答へ載せてはならない: テナント管理者へ返すと
/// 他テナントの secret 総数が漏れる）。
///
/// 運用上の使い方: KEK を切り替えたあと `POST /admin/secrets/rekey` を回し、旧 kid の gauge が
/// 0 になったら `SECRETS_RETIRED_KEYS` から旧鍵を撤去してよい（0 になる前の撤去は復号不能
/// ＝ データ喪失）。
pub async fn run_secret_kid_gauge(state: AppState, interval_secs: u64) {
    let period = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(
        interval_secs = period.as_secs(),
        "secret KEK kid gauge updater started"
    );

    loop {
        ticker.tick().await;
        match crate::db::secrets_kek_kid_counts_all(state.pool()).await {
            Ok(counts) => {
                // 前周期の kid が消えても gauge が残らないよう、毎回リセットしてから set する。
                state.metrics().secret_versions_by_kid.reset();
                for (kid, n) in counts {
                    state
                        .metrics()
                        .secret_versions_by_kid
                        .with_label_values(&[kid.as_str()])
                        .set(n);
                }
            }
            Err(e) => {
                // best-effort: 観測の失敗で本流を止めない（次周期で再試行）。
                tracing::warn!(error = %e, "failed to refresh secret KEK kid gauge");
            }
        }
    }
}
