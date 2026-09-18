//! Executions persistence.
use hibana_database::prelude::*;

use super::saturating_i64;
use chrono::DateTime;
use chrono::NaiveDate;
use chrono::Utc;
use hibana_shared::ExecutionStatus;
use hibana_shared::UsageMetrics;
use sea_orm::sea_query::extension::postgres::PgExpr;
use serde_json::Value;

/// Select only the status from HTTP result metadata, never the response body.
pub fn http_status_expression() -> Expr {
    Expr::case(
        Condition::all()
            .add(executions::Column::HttpRequest.eq(true))
            .add(executions::Column::Status.eq("succeeded")),
        Expr::col(executions::Column::Output).get_json_field("status"),
    )
    .finally(Expr::val(None::<Value>))
    .into()
}

pub fn http_status_code(value: Option<&Value>) -> Option<u16> {
    value
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .map(|status| status as u16)
}

pub fn execution_errors_condition() -> Condition {
    Condition::any()
        .add(executions::Column::Status.is_in(["failed", "timeout"]))
        .add(http_status_expression().between(serde_json::json!(400), serde_json::json!(599)))
}

/// Old Control Planes can still finish requests after the retention migration.
/// Sweep those inputs repeatedly, including after rolling upgrades. Bound each
/// transaction and use the retained-input index instead of scanning all history.
pub async fn purge_terminal_execution_inputs(
    tx: &DatabaseTransaction,
    tenant_id: &str,
) -> Result<u64, DbErr> {
    let terminal = Condition::all()
        .add(executions::Column::TenantId.eq(tenant_id))
        .add(executions::Column::HttpRequest.eq(true))
        .add(executions::Column::Status.is_not_in(["pending", "running"]))
        .add(
            Condition::any()
                .add(executions::Column::Input.is_not_null())
                .add(executions::Column::InputRef.is_not_null()),
        );
    let batch = executions::Entity::find()
        .select_only()
        .column(executions::Column::Id)
        .filter(terminal.clone())
        .order_by_asc(executions::Column::Id)
        .limit(500)
        .into_query();
    Ok(executions::Entity::update_many()
        .col_expr(executions::Column::Input, Expr::val(None::<Value>))
        .col_expr(executions::Column::InputRef, Expr::val(None::<String>))
        .filter(terminal)
        .filter(executions::Column::Id.in_subquery(batch))
        .exec(tx)
        .await?
        .rows_affected)
}

/// Execution history projection. Never fetch request/response payloads or their references.
#[derive(Debug, Clone, FromQueryResult)]
pub struct ExecutionRow {
    pub id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub http_status: Option<Value>,
    pub error: Option<Value>,
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
        .column_as(http_status_expression(), "http_status")
        .columns([
            executions::Column::Id,
            executions::Column::TenantId,
            executions::Column::ComponentId,
            executions::Column::VersionId,
            executions::Column::Status,
            executions::Column::Error,
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

/// 同時実行枠を使用している pending/running の HTTP 実行数。
/// 受付時はテナントの admission ロックを取得し、同一トランザクションで集計・挿入する。
/// FORCE RLS 下のため、事前に `set_tenant_guc(tenant)` を同一 tx に設定する。
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
        .col_expr(executions::Column::Input, Expr::val(None::<Value>))
        .col_expr(executions::Column::InputRef, Expr::val(None::<String>))
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
        .col_expr(executions::Column::Input, Expr::val(None::<Value>))
        .col_expr(executions::Column::InputRef, Expr::val(None::<String>))
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
