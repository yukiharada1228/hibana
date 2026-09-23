//! Signed HTTP completion with one provenance check and accounting transaction.
use crate::{db, error::AppError, state::AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use hibana_shared::http::MAX_RESPONSE_BYTES;
use hibana_shared::{FaasError, ResourceLimits, ResultMessage, UsageMetrics};
use sea_orm::TransactionTrait as _;
use serde_json::json;

/// Retrying a result is safe; it never re-executes the guest or counts usage twice.
pub(crate) async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(result): Json<ResultMessage>,
) -> Result<StatusCode, AppError> {
    // Suspension and expiry stop admission, not completion of admitted work.
    let claims = crate::job_auth::verified_claims(&state, &headers)?;
    if claims.tenant_id != result.tenant_id
        || claims.execution_id != result.execution_id
        || headers
            .get("x-hibana-job-token")
            .and_then(|v| v.to_str().ok())
            != Some(result.job_token.as_str())
        || !result.status.is_terminal()
    {
        return Err(FaasError::Unauthorized.into());
    }
    tracing::Span::current().record("execution_id", result.execution_id.as_str());

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &claims.tenant_id).await?;
    // Lock the exact HTTP execution bound to the signed claims. Concurrent
    // completions and the sweeper must observe its committed terminal status.
    let (component_id, status) = db::lock_http_completion(
        &tx,
        &claims.tenant_id,
        &claims.execution_id,
        &claims.version_id,
    )
    .await?
    .ok_or(FaasError::Unauthorized)?;
    if status == result.status.as_str() {
        tx.commit().await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    if !matches!(status.as_str(), "pending" | "running") {
        return Err(FaasError::Conflict("HTTP result rejected".into()).into());
    }

    // Workers are outside the trust boundary. Bound diagnostics for JSONB and
    // clamp reported usage against the pinned version's authoritative limits.
    let error = result
        .error
        .as_ref()
        .map(|msg| json!({ "message": hibana_shared::diagnostics::format_error(msg) }));
    let usage = match result.usage {
        Some(raw) => {
            match db::find_version_resource_limits(&tx, &claims.tenant_id, &claims.version_id)
                .await?
            {
                Some(limits) => Some(clamp_usage(&raw, &limits)),
                None => {
                    tracing::warn!(
                        execution_id = %claims.execution_id,
                        version_id = %claims.version_id,
                        "authoritative resource limits unavailable; dropping untrusted usage"
                    );
                    None
                }
            }
        }
        None => None,
    };
    let outcome = db::finalize_execution(
        &tx,
        &claims.tenant_id,
        &claims.execution_id,
        result.status,
        result.output.as_ref(),
        error.as_ref(),
        usage.as_ref(),
    )
    .await?
    .ok_or_else(|| FaasError::Conflict("HTTP result rejected".into()))?;
    if let Some(logs) = &result.logs {
        db::save_application_logs(&tx, &claims.tenant_id, &claims.execution_id, logs).await?;
    }
    db::upsert_usage_rollup(
        &tx,
        &claims.tenant_id,
        &component_id,
        outcome.period_start,
        result.status,
        &usage.unwrap_or_default(),
    )
    .await?;
    tx.commit().await?;

    // Observe only committed transitions, including for the duration histogram.
    state
        .metrics()
        .execution_duration_seconds
        .with_label_values(&[result.status.as_str()])
        .observe(outcome.duration_secs);
    state
        .metrics()
        .executions_total
        .with_label_values(&[result.status.as_str()])
        .inc();
    Ok(StatusCode::NO_CONTENT)
}

/// worker 自己申告の計量を実行・HTTP 応答の上限で sanity clamp する (M5, §15)。
///
/// worker は署名鍵を持たない**非特権・信頼境界外**ランタイム (§3.3) なので、申告値は無検証だと
/// 課金水増しや桁あふれで rollup の SUM を破壊しうる。各指標を version の resource_limits（worker の
/// `get_limits` と同じ権威値）または HTTP 応答の上限で頭打ちにしてから永続化する:
/// - `wall_time_ms`  → `max_execution_time_ms`（実行に許される総壁時計上限）
/// - `cpu_fuel_used` → `max_fuel`（fuel 無効化時は計測できないため 0）
/// - `peak_memory_bytes` → `max_memory_bytes`（StoreLimits が強制する上限）
/// - `output_bytes`  → `MAX_RESPONSE_BYTES`（ストリーミングの累計はゲストメモリ上限を超え得る）
///
/// 純関数（DB 不要）で clamp ロジックをテスト可能にする。`UsageMetrics` は `Copy`。
fn clamp_usage(raw: &UsageMetrics, limits: &ResourceLimits) -> UsageMetrics {
    UsageMetrics {
        cpu_fuel_used: raw.cpu_fuel_used.min(limits.max_fuel.unwrap_or(0)),
        wall_time_ms: raw.wall_time_ms.min(limits.max_execution_time_ms),
        peak_memory_bytes: raw.peak_memory_bytes.min(limits.max_memory_bytes),
        output_bytes: raw.output_bytes.min(MAX_RESPONSE_BYTES as u64),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_limits() -> ResourceLimits {
        ResourceLimits {
            max_memory_bytes: 1024,
            max_wall_time_ms: 0,
            max_execution_time_ms: 500,
            max_fuel: Some(1_000),
        }
    }

    /// 上限超過の自己申告値は各上限に頭打ちされる（課金水増し防止）。
    #[test]
    fn clamp_usage_clamps_each_field_to_limit() {
        let limits = test_limits();
        let raw = UsageMetrics {
            cpu_fuel_used: 5_000,         // > max_fuel 1_000
            wall_time_ms: 9_999,          // > max_execution_time_ms 500
            peak_memory_bytes: 1_000_000, // > max_memory_bytes 1024
            output_bytes: u64::MAX,       // > MAX_RESPONSE_BYTES
        };
        let c = clamp_usage(&raw, &limits);
        assert_eq!(c.cpu_fuel_used, 1_000);
        assert_eq!(c.wall_time_ms, 500);
        assert_eq!(c.peak_memory_bytes, 1024);
        assert_eq!(c.output_bytes, MAX_RESPONSE_BYTES as u64);
    }

    #[test]
    fn clamp_usage_preserves_streamed_output_larger_than_guest_memory() {
        let limits = test_limits();
        for output_bytes in [limits.max_memory_bytes + 1, MAX_RESPONSE_BYTES as u64] {
            let raw = UsageMetrics {
                output_bytes,
                ..Default::default()
            };
            assert_eq!(clamp_usage(&raw, &limits), raw);
        }
    }

    /// 上限以下の値はそのまま通す（正規の計測値を歪めない）。
    #[test]
    fn clamp_usage_passes_through_in_range_values() {
        let limits = test_limits();
        let raw = UsageMetrics {
            cpu_fuel_used: 800,
            wall_time_ms: 250,
            peak_memory_bytes: 512,
            output_bytes: 128,
        };
        assert_eq!(clamp_usage(&raw, &limits), raw);
    }

    /// 0 は 0 のまま（計測して 0 を歪めない）。
    #[test]
    fn clamp_usage_zero_stays_zero() {
        let c = clamp_usage(&UsageMetrics::default(), &test_limits());
        assert_eq!(c, UsageMetrics::default());
    }

    /// Unmetered fuel cannot be reported as measured CPU usage by a Worker.
    #[test]
    fn clamp_usage_fuel_disabled_discards_unmetered_cpu() {
        let limits = ResourceLimits {
            max_fuel: None,
            ..test_limits()
        };
        let raw = UsageMetrics {
            cpu_fuel_used: u64::MAX,
            ..Default::default()
        };
        let c = clamp_usage(&raw, &limits);
        assert_eq!(c.cpu_fuel_used, 0);
    }
}
