//! Usage persistence.
use super::saturating_i64;
use chrono::NaiveDate;
use faas_shared::ExecutionStatus;
use faas_shared::UsageMetrics;
use sqlx::Row;

pub async fn upsert_usage_rollup(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    period_start: NaiveDate,
    status: ExecutionStatus,
    usage: &UsageMetrics,
) -> Result<(), sqlx::Error> {
    debug_assert!(status.is_terminal());

    let succeeded_count: i64 = (status == ExecutionStatus::Succeeded) as i64;
    let failed_count: i64 = (status == ExecutionStatus::Failed) as i64;
    let timeout_count: i64 = (status == ExecutionStatus::Timeout) as i64;

    sqlx::query(UPSERT_USAGE_ROLLUP_SQL)
        .bind(tenant_id)
        .bind(period_start)
        .bind(component_id)
        .bind(saturating_i64(usage.cpu_fuel_used))
        .bind(saturating_i64(usage.wall_time_ms))
        .bind(saturating_i64(usage.peak_memory_bytes))
        .bind(saturating_i64(usage.output_bytes))
        .bind(succeeded_count)
        .bind(failed_count)
        .bind(timeout_count)
        .execute(executor)
        .await?;

    Ok(())
}

/// `upsert_usage_rollup` が発行する増分 UPSERT SQL。複合 PK を競合ターゲットに、SUM 列は加算、
/// `peak_memory_bytes_max` は `GREATEST`（MAX セマンティクス）で更新する。定数に切り出して
/// DB 非依存のユニットテストで集計セマンティクスを静的検査できるようにする。
pub(super) const UPSERT_USAGE_ROLLUP_SQL: &str = "INSERT INTO usage_rollups \
     (tenant_id, period_start, component_id, invocation_count, cpu_fuel_used, wall_time_ms, \
      peak_memory_bytes_max, output_bytes, succeeded_count, failed_count, timeout_count) \
     VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (tenant_id, period_start, component_id) DO UPDATE SET \
       invocation_count = usage_rollups.invocation_count + 1, \
       cpu_fuel_used = usage_rollups.cpu_fuel_used + EXCLUDED.cpu_fuel_used, \
       wall_time_ms = usage_rollups.wall_time_ms + EXCLUDED.wall_time_ms, \
       peak_memory_bytes_max = GREATEST(usage_rollups.peak_memory_bytes_max, EXCLUDED.peak_memory_bytes_max), \
       output_bytes = usage_rollups.output_bytes + EXCLUDED.output_bytes, \
       succeeded_count = usage_rollups.succeeded_count + EXCLUDED.succeeded_count, \
       failed_count = usage_rollups.failed_count + EXCLUDED.failed_count, \
       timeout_count = usage_rollups.timeout_count + EXCLUDED.timeout_count, \
       updated_at = now()";

#[derive(Debug, Clone)]
pub struct UsageRollupRow {
    pub component_id: String,
    pub invocation_count: i64,
    pub cpu_fuel_used: i64,
    pub wall_time_ms: i64,
    pub peak_memory_bytes_max: i64,
    pub output_bytes: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timeout_count: i64,
}

/// `GET /usage` 用に `usage_rollups` を期間集計する (M5, §15 / §6.0, read スコープ)。
///
/// `period_start`（UTC 日境界）が `[from, to]`（両端含む）にある行を component 別に集約する。
/// `tenant_id` は RLS の GUC（`set_tenant_guc`）と二重防御で WHERE にもバインドする（既存 db
/// クエリ規約。文字列結合はしない: injection 防止）。`component_id` が `Some` なら単一 component に
/// 絞り込む（`$4::text IS NULL OR ...` で NULL なら全件）。totals はハンドラ側で本行を畳んで算出する。
pub async fn get_usage_rollups(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    from: NaiveDate,
    to: NaiveDate,
    component_id: Option<&str>,
) -> Result<Vec<UsageRollupRow>, sqlx::Error> {
    let rows = sqlx::query(
        // SUM(bigint) は Postgres では NUMERIC を返すため、各集計を ::bigint へ明示キャストして
        // Rust 側の i64 デコード（UsageRollupRow）と型を一致させる（MAX は元の bigint を保つ）。
        // 行数 × 各列とも実運用域では i64 に収まる（saturating_i64 で書込時に頭打ち済み）。
        "SELECT component_id, \
                SUM(invocation_count)::bigint AS invocation_count, \
                SUM(cpu_fuel_used)::bigint AS cpu_fuel_used, \
                SUM(wall_time_ms)::bigint AS wall_time_ms, \
                MAX(peak_memory_bytes_max) AS peak_memory_bytes_max, \
                SUM(output_bytes)::bigint AS output_bytes, \
                SUM(succeeded_count)::bigint AS succeeded_count, \
                SUM(failed_count)::bigint AS failed_count, \
                SUM(timeout_count)::bigint AS timeout_count \
         FROM usage_rollups \
         WHERE tenant_id = $1 AND period_start >= $2 AND period_start <= $3 \
           AND ($4::text IS NULL OR component_id = $4) \
         GROUP BY component_id \
         ORDER BY component_id",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(UsageRollupRow {
                component_id: r.try_get("component_id")?,
                invocation_count: r.try_get("invocation_count")?,
                cpu_fuel_used: r.try_get("cpu_fuel_used")?,
                wall_time_ms: r.try_get("wall_time_ms")?,
                peak_memory_bytes_max: r.try_get("peak_memory_bytes_max")?,
                output_bytes: r.try_get("output_bytes")?,
                succeeded_count: r.try_get("succeeded_count")?,
                failed_count: r.try_get("failed_count")?,
                timeout_count: r.try_get("timeout_count")?,
            })
        })
        .collect()
}
