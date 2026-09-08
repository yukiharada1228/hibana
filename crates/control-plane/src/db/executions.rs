//! Executions persistence.
use super::saturating_i64;
use chrono::DateTime;
use chrono::NaiveDate;
use chrono::Utc;
use hibana_shared::ExecutionStatus;
use hibana_shared::UsageMetrics;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

/// `executions` の 1 行 (GET /executions/{id} 応答)。
#[derive(Debug, Clone)]
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

impl ExecutionRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            tenant_id: row.try_get("tenant_id")?,
            component_id: row.try_get("component_id")?,
            version_id: row.try_get("version_id")?,
            status: row.try_get("status")?,
            input: row.try_get("input")?,
            output: row.try_get("output")?,
            error: row.try_get("error")?,
            input_ref: row.try_get("input_ref")?,
            output_ref: row.try_get("output_ref")?,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    }
}

pub async fn find_execution_provenance(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<(String, String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT component_id, version_id, status FROM executions \
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok((
            r.try_get::<String, _>("component_id")?,
            r.try_get::<String, _>("version_id")?,
            r.try_get::<String, _>("status")?,
        ))
    })
    .transpose()
}

/// 実行を取得する。
pub async fn get_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<ExecutionRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, tenant_id, component_id, version_id, status, input, output, error, \
                input_ref, output_ref, created_at, started_at, finished_at \
         FROM executions WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await?;

    row.as_ref().map(ExecutionRow::from_row).transpose()
}

/// Secret authorization needs provenance and creation time, never the HTTP body.
/// Keep this projection small even when the execution contains a large upload.
#[derive(sqlx::FromRow)]
pub struct SecretExecution {
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub async fn secret_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
) -> Result<Option<SecretExecution>, sqlx::Error> {
    sqlx::query_as(
        "SELECT component_id, version_id, status, created_at FROM executions \
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant_id)
    .bind(execution_id)
    .fetch_optional(executor)
    .await
}

/// 当該 component を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_component(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS ( \
            SELECT 1 FROM executions \
            WHERE tenant_id = $1 AND component_id = $2 \
              AND http_request AND status IN ('pending', 'running') \
         ) AS present",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

/// 当該 version を参照する pending/running の execution が存在するか (§6.7 削除保護)。
pub async fn has_active_executions_for_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    version_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS ( \
            SELECT 1 FROM executions \
            WHERE tenant_id = $1 AND version_id = $2 \
              AND http_request AND status IN ('pending', 'running') \
         ) AS present",
    )
    .bind(tenant_id)
    .bind(version_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

/// reaper 用: 当該テナントの in-flight（pending/running）execution 数を数える (M3d, §8)。
///
/// **DB COUNT は in-flight 同時実行数の唯一の真実**であり、Redis カウンタはその速い近似に
/// 過ぎない。reaper はこの値で共有カウンタを上書き再同期し、終端パスでの DECR 取りこぼし／
/// 二重 DECR によるドリフトを定期的に消す。executions は FORCE RLS 下にあるため、呼び出し側は
/// 事前に `set_tenant_guc(tenant)` を同一 tx に設定していること（GUC 未設定は fail-closed ERROR）。
pub async fn count_inflight_executions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM executions \
         WHERE tenant_id = $1 AND http_request AND status IN ('pending', 'running')",
    )
    .bind(tenant_id)
    .fetch_one(executor)
    .await?;
    row.try_get("n")
}

pub async fn finalize_stuck_executions(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    deadline_secs: i64,
) -> Result<Vec<SweptExecution>, sqlx::Error> {
    let rows = sqlx::query(STUCK_EXECUTION_SWEEP_SQL)
        .bind(tenant_id)
        .bind(deadline_secs)
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SweptExecution {
                id: r.try_get::<String, _>("id")?,
                component_id: r.try_get::<String, _>("component_id")?,
                period_start: r.try_get::<NaiveDate, _>("period_start")?,
            })
        })
        .collect()
}

/// `finalize_stuck_executions` が CAS で `failed` 化した 1 行。sweeper 経路の rollup 計上に必要な
/// 最小情報（集計キー）を持つ (M5, §15)。
#[derive(Debug, Clone)]
pub struct SweptExecution {
    pub id: String,
    pub component_id: String,
    /// この UPDATE が打った `finished_at`（DB の `now()`）由来の UTC 日。`usage_rollups.period_start`。
    pub period_start: NaiveDate,
}

/// `finalize_stuck_executions` の SQL。`created_at < now() - deadline` の非終端（pending/running）
/// 行のみを `failed` に遷移させ（CAS: 既終端は WHERE で除外）、遷移した行の集計キーを返す。
/// `$2` は INTERVAL の秒数。`error` には sweeper 由来であることを記録する（監査・調査用）。
/// M5: rollup 計上のため `component_id` と `period_start`（finished_at 由来 UTC 日, 単一時計源）も返す。
pub(super) const STUCK_EXECUTION_SWEEP_SQL: &str = "UPDATE executions \
     SET status = 'failed', \
         error = '\"HTTP execution exceeded deadline without a terminal result (stuck-execution sweeper)\"'::jsonb, \
         finished_at = now() \
     WHERE tenant_id = $1 \
       AND http_request AND status IN ('pending', 'running') \
       AND created_at < now() - make_interval(secs => $2::double precision) \
     RETURNING id, component_id, (finished_at AT TIME ZONE 'UTC')::date AS period_start";

#[derive(Debug, Clone, Copy)]
pub struct FinalizeOutcome {
    /// `usage_rollups` の集計日（`finished_at` の UTC 日）。単一時計源（§15 M5）。
    pub period_start: NaiveDate,
    /// `finished_at - created_at`（秒）。`execution_duration_seconds` ヒストグラムに使う（M10 follow-up）。
    pub duration_secs: f64,
}

pub async fn finalize_execution(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    execution_id: &str,
    status: ExecutionStatus,
    output: Option<&Value>,
    error: Option<&Value>,
    usage: Option<&UsageMetrics>,
) -> Result<Option<FinalizeOutcome>, sqlx::Error> {
    debug_assert!(status.is_terminal());

    // 計量列は nullable（BIGINT/INTEGER）。usage None のときは全て NULL バインドする。
    // u64 → i64 の格納は clamp 済み前提だが、念のため飽和でオーバーフローを避ける。
    let cpu_fuel_used = usage.map(|u| saturating_i64(u.cpu_fuel_used));
    let wall_time_ms = usage.map(|u| saturating_i64(u.wall_time_ms));
    let peak_memory_bytes = usage.map(|u| saturating_i64(u.peak_memory_bytes));
    let output_bytes = usage.map(|u| saturating_i64(u.output_bytes));
    let invocation_count: Option<i32> = usage.map(|_| 1);

    // RETURNING を `fetch_optional` で受ける: CAS が当たれば 1 行（period_start: UTC 日）、
    // 既終端で当たらなければ 0 行（None）。rows_affected を数える代わりに、遷移有無と
    // 集計用日付を **1 ステートメント**で同時に得る（二重計上の冪等アンカーは不変）。
    let row = sqlx::query(FINALIZE_EXECUTION_SQL)
        .bind(tenant_id)
        .bind(execution_id)
        .bind(status.as_str())
        .bind(output)
        .bind(error)
        .bind(cpu_fuel_used)
        .bind(wall_time_ms)
        .bind(peak_memory_bytes)
        .bind(output_bytes)
        .bind(invocation_count)
        .fetch_optional(executor)
        .await?;

    // CAS が当たった行だけ RETURNING が返る（既終端は 0 行 = None）。二重計上の冪等アンカーは不変。
    row.map(|r| {
        use sqlx::Row as _;
        Ok(FinalizeOutcome {
            period_start: r.try_get("period_start")?,
            // created_at より前に finished_at になることは無いが、時計の巻き戻し等で負値が来ても
            // ヒストグラムに負を渡さないよう 0 で下限を切る。
            duration_secs: r.try_get::<f64, _>("duration_secs")?.max(0.0),
        })
    })
    .transpose()
}

/// `finalize_execution` が発行する SQL。終端遷移は **CAS**（compare-and-set）で行う:
/// `WHERE ... AND status NOT IN (terminal)` により、既に終端の行には 0 行しか当たらない
/// （= 重複 result の再配送や worker 多重実行があっても終端状態を上書きしない, §6.6）。
/// 定数に切り出して DB 非依存のユニットテストで CAS ガードの存在を検査できるようにする。
/// M5 (§15): per-execution 計量列（$6..$10）を同一 SET 句に同梱する。CAS ガード
/// （`WHERE ... AND status NOT IN (terminal)`）は不変。これにより重複 result は 0 行更新となり、
/// 計量列も同時に no-op になる（status と計量が同一行・同一述語で原子更新されるため、status だけ no-op で
/// 計量だけ書かれる窓が存在しない＝二重計上が構造的に不可能）。
pub(super) const FINALIZE_EXECUTION_SQL: &str = "UPDATE executions \
     SET status = $3, output = $4, error = $5, finished_at = now(), \
         cpu_fuel_used = $6, wall_time_ms = $7, peak_memory_bytes = $8, \
         output_bytes = $9, invocation_count = $10 \
     WHERE tenant_id = $1 AND id = $2 \
       AND status NOT IN ('succeeded', 'failed', 'timeout') \
     RETURNING (finished_at AT TIME ZONE 'UTC')::date AS period_start, \
               EXTRACT(EPOCH FROM (finished_at - created_at))::float8 AS duration_secs";
