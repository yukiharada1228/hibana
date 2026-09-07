//! Usage management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use faas_shared::FaasError;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GET /usage — テナント利用量参照 (M5, §15 / §6.0, read スコープ)
// ---------------------------------------------------------------------------

/// `GET /usage` のクエリパラメータ。
///
/// `from`/`to` は `YYYY-MM-DD`（UTC 日境界）。`usage_rollups` は UTC 日次粒度で集計されるため
/// レスポンスも UTC 日次（period の両端含む）になる。既定は `to`=今日(UTC) / `from`=`to`-30 日。
/// `component_id` 指定時はその component のみに絞り込む（未指定なら全 component）。
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub component_id: Option<String>,
}

/// `GET /usage` 応答。`totals` は `by_component` を畳んだ全体集計（SUM 列は和、peak は max）。
#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub tenant_id: String,
    /// 集計対象期間の開始（UTC 日, 含む）。`YYYY-MM-DD`。
    pub from: String,
    /// 集計対象期間の終了（UTC 日, 含む）。`YYYY-MM-DD`。
    pub to: String,
    pub totals: UsageTotals,
    pub by_component: Vec<UsageByComponent>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
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

/// component 別の集計 1 件。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UsageByComponent {
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

/// `from`/`to` クエリ文字列を `NaiveDate` に解決する（純関数・時計を引数化してテスト可能）。
///
/// 既定: `to`=`today` / `from`=`to`-30 日。指定時は `YYYY-MM-DD` をパースし、不正は
/// [`FaasError::InvalidRequest`]（→ 400）。`from > to` も 400 で弾く（空でない範囲を保証）。
pub(super) fn resolve_usage_range(
    from: Option<&str>,
    to: Option<&str>,
    today: chrono::NaiveDate,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate), FaasError> {
    let parse = |label: &str, s: &str| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| FaasError::InvalidRequest(format!("{label} must be YYYY-MM-DD")))
    };

    let to = match to {
        Some(s) => parse("to", s)?,
        None => today,
    };
    let from = match from {
        Some(s) => parse("from", s)?,
        None => to - chrono::Duration::days(30),
    };

    if from > to {
        return Err(FaasError::InvalidRequest(
            "from must not be after to".into(),
        ));
    }
    Ok((from, to))
}

/// `by_component` を期間全体の `totals` へ畳む（純関数）。SUM 列は和、peak は max を取る。
pub(super) fn fold_usage_totals(by_component: &[UsageByComponent]) -> UsageTotals {
    by_component.iter().fold(
        UsageTotals {
            invocation_count: 0,
            cpu_fuel_used: 0,
            wall_time_ms: 0,
            peak_memory_bytes_max: 0,
            output_bytes: 0,
            succeeded_count: 0,
            failed_count: 0,
            timeout_count: 0,
        },
        |mut acc, c| {
            acc.invocation_count += c.invocation_count;
            acc.cpu_fuel_used += c.cpu_fuel_used;
            acc.wall_time_ms += c.wall_time_ms;
            acc.peak_memory_bytes_max = acc.peak_memory_bytes_max.max(c.peak_memory_bytes_max);
            acc.output_bytes += c.output_bytes;
            acc.succeeded_count += c.succeeded_count;
            acc.failed_count += c.failed_count;
            acc.timeout_count += c.timeout_count;
            acc
        },
    )
}

/// GET /usage — テナント利用量参照 (M5, §15 / §6.0, read スコープ)。
///
/// `principal.tenant_id` を唯一の権威値として使う（cross-tenant path を持たず IDOR 面を増やさない）。
/// RLS 下では SELECT 実行接続に `app.tenant_id` をセットする必要があるため tx を張って GUC を設定する
/// （GUC 未設定の bare 接続は fail-closed で ERROR）。WHERE にも tenant_id をバインドして二重防御する。
pub async fn get_usage(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<UsageQuery>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let today = chrono::Utc::now().date_naive();
    let (from, to) = resolve_usage_range(q.from.as_deref(), q.to.as_deref(), today)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let rows = db::get_usage_rollups(&mut *tx, tenant, from, to, q.component_id.as_deref()).await?;
    tx.commit().await?;

    let by_component: Vec<UsageByComponent> = rows
        .into_iter()
        .map(|r| UsageByComponent {
            component_id: r.component_id,
            invocation_count: r.invocation_count,
            cpu_fuel_used: r.cpu_fuel_used,
            wall_time_ms: r.wall_time_ms,
            peak_memory_bytes_max: r.peak_memory_bytes_max,
            output_bytes: r.output_bytes,
            succeeded_count: r.succeeded_count,
            failed_count: r.failed_count,
            timeout_count: r.timeout_count,
        })
        .collect();

    let totals = fold_usage_totals(&by_component);

    Ok(Json(UsageResponse {
        tenant_id: tenant.clone(),
        from: from.format("%Y-%m-%d").to_string(),
        to: to.format("%Y-%m-%d").to_string(),
        totals,
        by_component,
    }))
}
