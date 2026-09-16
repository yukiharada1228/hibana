//! Executions persistence.
use hibana_database::prelude::*;

use super::saturating_i64;
use chrono::DateTime;
use chrono::NaiveDate;
use chrono::Utc;
use hibana_shared::ExecutionStatus;
use hibana_shared::UsageMetrics;
use serde_json::Value;

/// `executions` の 1 行 (GET /executions/{id} 応答)。
#[derive(Debug, Clone, FromQueryResult)]
pub struct ExecutionRow {
    pub id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<Value>,
    /// M3d (§3.4 / §6.4): 大入力の退避オブジェクトキー（NULL=インライン）。
    pub input_ref: Option<String>,
    /// M3d (§3.4 / §6.4): 大出力の退避オブジェクトキー（NULL=インライン）。
    pub output_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub async fn find_execution_provenance(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<(String, String, String)>, DbErr> {
    executions::Entity::find()
        .select_only()
        .columns([
            executions::Column::ComponentId,
            executions::Column::VersionId,
            executions::Column::Status,
        ])
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::Id.eq(execution_id))
        .into_tuple()
        .one(executor)
        .await
}

/// 実行を取得する。
pub async fn get_execution(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<ExecutionRow>, DbErr> {
    executions::Entity::find()
        .select_only()
        .columns([
            executions::Column::Id,
            executions::Column::TenantId,
            executions::Column::ComponentId,
            executions::Column::VersionId,
            executions::Column::Status,
            executions::Column::Input,
            executions::Column::Output,
            executions::Column::Error,
            executions::Column::InputRef,
            executions::Column::OutputRef,
            executions::Column::CreatedAt,
            executions::Column::StartedAt,
            executions::Column::FinishedAt,
        ])
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::Id.eq(execution_id))
        .into_model::<ExecutionRow>()
        .one(executor)
        .await
}

/// Secret authorization needs provenance and creation time, never the HTTP body.
/// Keep this projection small even when the execution contains a large upload.
#[derive(FromQueryResult)]
pub struct SecretExecution {
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub async fn secret_execution(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<SecretExecution>, DbErr> {
    executions::Entity::find()
        .select_only()
        .columns([
            executions::Column::ComponentId,
            executions::Column::VersionId,
            executions::Column::Status,
            executions::Column::CreatedAt,
        ])
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::Id.eq(execution_id))
        .into_model::<SecretExecution>()
        .one(executor)
        .await
}

/// 当該 component を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_component(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, DbErr> {
    Ok(executions::Entity::find()
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(executions::Column::Status.is_in(["pending", "running"]))
        .filter(executions::Column::ComponentId.eq(component_id))
        .count(executor)
        .await?
        > 0)
}

/// List versions protected by pending/running HTTP executions in one query.
pub async fn active_execution_version_ids(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<String>, DbErr> {
    executions::Entity::find()
        .select_only()
        .column(executions::Column::VersionId)
        .distinct()
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::ComponentId.eq(component_id))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(executions::Column::Status.is_in(["pending", "running"]))
        .into_tuple::<String>()
        .all(executor)
        .await
}

/// 当該 version を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    version_id: &str,
) -> Result<bool, DbErr> {
    Ok(executions::Entity::find()
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(executions::Column::Status.is_in(["pending", "running"]))
        .filter(executions::Column::VersionId.eq(version_id))
        .count(executor)
        .await?
        > 0)
}

/// reaper 用: 当該テナントの in-flight（pending/running）execution 数を数える (M3d, §8)。
///
/// **DB COUNT は in-flight 同時実行数の唯一の真実**であり、Redis カウンタはその速い近似に
/// 過ぎない。reaper はこの値で共有カウンタを上書き再同期し、終端パスでの DECR 取りこぼし／
/// 二重 DECR によるドリフトを定期的に消す。executions は FORCE RLS 下にあるため、呼び出し側は
/// 事前に `set_tenant_guc(tenant)` を同一 tx に設定していること（GUC 未設定は fail-closed ERROR）。
pub async fn count_inflight_executions(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<i64, DbErr> {
    Ok(std::cmp::min(
        executions::Entity::find()
            .filter(executions::Column::TenantId.eq(tenant_id))
            .filter(executions::Column::HttpRequest.eq(true))
            .filter(executions::Column::Status.is_in(["pending", "running"]))
            .count(executor)
            .await?,
        i64::MAX as u64,
    ) as i64)
}

pub async fn finalize_stuck_executions(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    deadline_secs: i64,
) -> Result<Vec<SweptExecution>, DbErr> {
    let error = serde_json::json!(
        "HTTP execution exceeded deadline without a terminal result (stuck-execution sweeper)"
    );
    let cutoff = now().sub(Expr::cust("interval '1 second'").mul(deadline_secs));
    let mut query = executions::Entity::update_many()
        .col_expr(executions::Column::Status, Expr::val("failed"))
        .col_expr(executions::Column::Error, Expr::val(error))
        .col_expr(executions::Column::FinishedAt, now())
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::HttpRequest.eq(true))
        .filter(executions::Column::Status.is_in(["pending", "running"]))
        .filter(Expr::col((executions::Entity, executions::Column::CreatedAt)).lt(cutoff))
        .into_query();
    query.returning(Query::returning().columns([
        executions::Column::Id,
        executions::Column::ComponentId,
        executions::Column::FinishedAt,
    ]));
    executor
        .query_all(&query)
        .await?
        .into_iter()
        .map(|r| {
            Ok(SweptExecution {
                id: r.try_get("", "id")?,
                component_id: r.try_get("", "component_id")?,
                period_start: r.try_get::<DateTime<Utc>>("", "finished_at")?.date_naive(),
            })
        })
        .collect()
}

/// `finalize_stuck_executions` が CAS で `failed` 化した 1 行。sweeper 経路の rollup 計上に必要な
/// 最小情報（集計キー）を持つ (M5, §15)。
#[derive(Debug, Clone, FromQueryResult)]
pub struct SweptExecution {
    pub id: String,
    pub component_id: String,
    /// この UPDATE が打った `finished_at`（DB の `now()`）由来の UTC 日。`usage_rollups.period_start`。
    pub period_start: NaiveDate,
}

#[derive(Debug, Clone, Copy)]
pub struct FinalizeOutcome {
    /// `usage_rollups` の集計日（`finished_at` の UTC 日）。単一時計源（§15 M5）。
    pub period_start: NaiveDate,
    /// `finished_at - created_at`（秒）。`execution_duration_seconds` ヒストグラムに使う（M10 follow-up）。
    pub duration_secs: f64,
}

pub async fn finalize_execution(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    execution_id: &str,
    status: ExecutionStatus,
    output: Option<&Value>,
    error: Option<&Value>,
    usage: Option<&UsageMetrics>,
) -> Result<Option<FinalizeOutcome>, DbErr> {
    debug_assert!(status.is_terminal());
    let mut query = executions::Entity::update_many()
        .col_expr(executions::Column::Status, Expr::val(status.as_str()))
        .col_expr(executions::Column::Output, Expr::val(output.cloned()))
        .col_expr(executions::Column::Error, Expr::val(error.cloned()))
        .col_expr(executions::Column::FinishedAt, now())
        .col_expr(
            executions::Column::CpuFuelUsed,
            Expr::val(usage.map(|u| saturating_i64(u.cpu_fuel_used))),
        )
        .col_expr(
            executions::Column::WallTimeMs,
            Expr::val(usage.map(|u| saturating_i64(u.wall_time_ms))),
        )
        .col_expr(
            executions::Column::PeakMemoryBytes,
            Expr::val(usage.map(|u| saturating_i64(u.peak_memory_bytes))),
        )
        .col_expr(
            executions::Column::OutputBytes,
            Expr::val(usage.map(|u| saturating_i64(u.output_bytes))),
        )
        .col_expr(
            executions::Column::InvocationCount,
            Expr::val(usage.map(|_| 1_i32)),
        )
        .filter(executions::Column::TenantId.eq(tenant_id))
        .filter(executions::Column::Id.eq(execution_id))
        .filter(executions::Column::Status.is_not_in(["succeeded", "failed", "timeout"]))
        .into_query();
    query.returning(Query::returning().columns([
        executions::Column::FinishedAt,
        executions::Column::CreatedAt,
    ]));
    executor
        .query_one(&query)
        .await?
        .map(|r| {
            let finished: DateTime<Utc> = r.try_get("", "finished_at")?;
            let created: DateTime<Utc> = r.try_get("", "created_at")?;
            Ok(FinalizeOutcome {
                period_start: finished.date_naive(),
                duration_secs: f64::max(
                    (finished - created).num_microseconds().unwrap_or(i64::MAX) as f64
                        / 1_000_000.0,
                    0.0,
                ),
            })
        })
        .transpose()
}
