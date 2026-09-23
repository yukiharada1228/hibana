//! Usage persistence.
use hibana_database::prelude::*;

use super::saturating_i64;
use chrono::NaiveDate;
use hibana_shared::ExecutionStatus;
use hibana_shared::UsageMetrics;

// Compute in PostgreSQL numeric before narrowing. Saturating individual events
// does not prevent BIGINT overflow when a day or a date range is accumulated.
fn bounded_counter(value: Expr) -> Expr {
    Func::least([value, Expr::val(i64::MAX)]).cast_as("bigint")
}

pub async fn upsert_usage_rollup(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    period_start: NaiveDate,
    status: ExecutionStatus,
    usage: &UsageMetrics,
) -> Result<(), DbErr> {
    debug_assert!(status.is_terminal());
    let mut conflict = OnConflict::columns([
        usage_rollups::Column::TenantId,
        usage_rollups::Column::PeriodStart,
        usage_rollups::Column::ComponentId,
    ]);
    for column in [
        usage_rollups::Column::InvocationCount,
        usage_rollups::Column::CpuFuelUsed,
        usage_rollups::Column::WallTimeMs,
        usage_rollups::Column::OutputBytes,
        usage_rollups::Column::SucceededCount,
        usage_rollups::Column::FailedCount,
        usage_rollups::Column::TimeoutCount,
    ] {
        conflict.value(
            column,
            bounded_counter(
                Expr::col((usage_rollups::Entity, column))
                    .cast_as("numeric")
                    .add(Expr::col(("excluded", column))),
            ),
        );
    }
    conflict
        .value(
            usage_rollups::Column::PeakMemoryBytesMax,
            Func::greatest([
                Expr::col((
                    usage_rollups::Entity,
                    usage_rollups::Column::PeakMemoryBytesMax,
                )),
                Expr::col(("excluded", usage_rollups::Column::PeakMemoryBytesMax)),
            ]),
        )
        .value(usage_rollups::Column::UpdatedAt, now());
    usage_rollups::Entity::insert(usage_rollups::ActiveModel {
        tenant_id: Set(tenant_id.into()),
        period_start: Set(period_start),
        component_id: Set(component_id.into()),
        invocation_count: Set(1_i64),
        cpu_fuel_used: Set(saturating_i64(usage.cpu_fuel_used)),
        wall_time_ms: Set(saturating_i64(usage.wall_time_ms)),
        peak_memory_bytes_max: Set(saturating_i64(usage.peak_memory_bytes)),
        output_bytes: Set(saturating_i64(usage.output_bytes)),
        succeeded_count: Set((status == ExecutionStatus::Succeeded) as i64),
        failed_count: Set((status == ExecutionStatus::Failed) as i64),
        timeout_count: Set((status == ExecutionStatus::Timeout) as i64),
        ..Default::default()
    })
    .on_conflict(conflict.to_owned())
    .exec_without_returning(executor)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, FromQueryResult)]
pub struct UsageRollupRow {
    pub component_id: String,
    #[serde(flatten)]
    #[sea_orm(nested)]
    pub usage: UsageTotals,
}

/// One shape for database aggregates, per-application usage and response totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, FromQueryResult)]
pub struct UsageTotals {
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
/// 絞り込む。totals はハンドラ側で本行を畳んで算出する。累計は BIGINT の上限で飽和させる。
pub async fn get_usage_rollups(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    from: NaiveDate,
    to: NaiveDate,
    component_id: Option<&str>,
) -> Result<Vec<UsageRollupRow>, DbErr> {
    let mut query = usage_rollups::Entity::find()
        .select_only()
        .column(usage_rollups::Column::ComponentId)
        .column_as(
            usage_rollups::Column::PeakMemoryBytesMax.max(),
            "peak_memory_bytes_max",
        )
        .filter(usage_rollups::Column::TenantId.eq(tenant_id))
        .filter(usage_rollups::Column::PeriodStart.between(from, to));
    if let Some(id) = component_id {
        query = query.filter(usage_rollups::Column::ComponentId.eq(id));
    }
    for (column, name) in [
        (usage_rollups::Column::InvocationCount, "invocation_count"),
        (usage_rollups::Column::CpuFuelUsed, "cpu_fuel_used"),
        (usage_rollups::Column::WallTimeMs, "wall_time_ms"),
        (usage_rollups::Column::OutputBytes, "output_bytes"),
        (usage_rollups::Column::SucceededCount, "succeeded_count"),
        (usage_rollups::Column::FailedCount, "failed_count"),
        (usage_rollups::Column::TimeoutCount, "timeout_count"),
    ] {
        query = query.column_as(bounded_counter(column.sum()), name);
    }
    query
        .group_by(usage_rollups::Column::ComponentId)
        .order_by_asc(usage_rollups::Column::ComponentId)
        .into_model::<UsageRollupRow>()
        .all(executor)
        .await
}
