use sea_orm::TransactionTrait as _;
use std::time::Duration;

use crate::state::AppState;

/// reaper ループ。`main` から `tokio::spawn` される。
///
/// `interval_secs` 周期で全テナントの入力清掃・孤立行回収を行う。
/// `stuck_deadline_secs` は孤立 pending/running 行を failed 化する deadline（0 で sweep 無効）。
pub async fn run(state: AppState, interval_secs: u64, stuck_deadline_secs: u64) {
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
        if let Err(e) = reconcile_once(&state, stuck_deadline_secs).await {
            // best-effort: 周期内の失敗は記録のみ。次の周期で再試行する。
            tracing::warn!(error = %e, "in-flight reaper pass failed; will retry next interval");
        }
    }
}

/// 1 周期分の入力清掃・孤立実行回収。停止中のテナントも処理する。
///
/// テナント列挙の失敗（DB 障害）はパス全体を失敗扱いにする（次周期で再試行）。個々の
/// テナントの入力清掃・回収失敗はそのテナントだけスキップし、他テナントの処理は続ける
/// （1 テナントの一時障害が全体を止めない）。
pub(crate) async fn reconcile_once(
    state: &AppState,
    stuck_deadline_secs: u64,
) -> anyhow::Result<()> {
    // tenants は RLS 無し → GUC 不要でプールから直接列挙できる。
    let tenants = crate::db::list_tenants_for_admin(state.pool()).await?;

    let mut reconciled = 0usize;
    for (tenant, _) in &tenants {
        match reconcile_tenant(state, tenant, stuck_deadline_secs).await {
            Ok(()) => reconciled += 1,
            Err(e) => {
                // DB エラーは当該テナントのみスキップし、次周期で再試行する。
                tracing::warn!(tenant = %tenant, error = %e, "reaper: failed to reconcile tenant");
            }
        }
    }

    // M4a (§3.8): 直近 1 周期で扱ったテナント数を gauge にスナップする（回収成否を問わず
    // 列挙できたテナント数）。reaper の停滞を可視化する 1 シグナルとして使う。
    state
        .metrics()
        .reaper_tenants_last
        .set(tenants.len() as i64);

    tracing::debug!(
        tenants = tenants.len(),
        reconciled,
        "in-flight reaper pass complete"
    );
    Ok(())
}

/// 1 テナント分の入力清掃・孤立実行回収。終端状態のコミットで同時実行枠も解放される。
async fn reconcile_tenant(
    state: &AppState,
    tenant: &str,
    stuck_deadline_secs: u64,
) -> anyhow::Result<()> {
    // executions は FORCE RLS 下 → 清掃と回収は GUC を設定した同一 tx で実行する。
    let tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&tx, tenant).await?;

    // Also runs when stuck-job recovery is disabled. A completed request must
    // not retain credentials just because an older Pod published its result.
    crate::db::purge_terminal_execution_inputs(&tx, tenant).await?;

    // (1) stuck-execution sweep（§8 リーク回収）。deadline 0 ならスキップ。
    //     deadline を過ぎても終端化されない pending/running 行を failed に倒し、回収数を得る。
    let swept = if stuck_deadline_secs > 0 {
        crate::db::finalize_stuck_executions(&tx, tenant, stuck_deadline_secs as i64).await?
    } else {
        Vec::new()
    };

    for row in &swept {
        crate::db::upsert_usage_rollup(
            &tx,
            tenant,
            &row.component_id,
            row.period_start,
            hibana_shared::ExecutionStatus::Failed,
            &hibana_shared::UsageMetrics::default(),
        )
        .await?;
    }

    tx.commit().await?;

    crate::store::tail::publish_completed(
        state.store(),
        tenant,
        &swept
            .iter()
            .map(|row| (row.component_id.as_str(), row.id.as_str()))
            .collect::<Vec<_>>(),
    )
    .await;

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
    }
    // Ten short batches can keep up with the default 50 requests/s at a 30s
    // interval. One 500-row batch alone could only retire 16 logs/s. Commit
    // each batch separately and cap the pass so one tenant cannot monopolize it.
    for _ in 0..10 {
        let tx = state.pool().begin().await?;
        crate::db::set_tenant_guc(&tx, tenant).await?;
        let removed = crate::db::purge_expired_application_logs(&tx, tenant).await?;
        tx.commit().await?;
        if removed < crate::db::LOG_CLEANUP_BATCH_SIZE {
            break;
        }
    }
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
/// 現在世代と未完了の実行が参照する世代を数える。全 CP の書込み鍵を切り替えてから
/// rekey し、受付停止・drain 後に最新の集計を確認して旧鍵をランタイム設定から外す。
/// 周期集計なので、古い gauge の 0 だけを撤去判断に使わない。バックアップ用の鍵は別途保持する。
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
