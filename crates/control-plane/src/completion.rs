//! Signed HTTP completion with a single CAS transition and accounting transaction.
use crate::state::AppState;
use faas_shared::{ExecutionStatus, JobClaims, ResourceLimits, ResultMessage, UsageMetrics};
use serde_json::json;
pub(crate) async fn handle_message(
    state: &AppState,
    tenant: &str,
    payload: &[u8],
) -> anyhow::Result<()> {
    let result: ResultMessage = serde_json::from_slice(payload)?;
    tracing::Span::current().record("execution_id", result.execution_id.as_str());

    // (1) 終端状態のみ反映する。
    if !result.status.is_terminal() {
        tracing::warn!(
            execution_id = %result.execution_id,
            status = %result.status,
            "ignoring non-terminal result message"
        );
        return Ok(());
    }

    // (2) job_token 在否。空（旧 worker / 欠落）は fail-closed で drop+audit。
    if result.job_token.is_empty() {
        tracing::warn!(
            execution_id = %result.execution_id,
            subject_tenant = %tenant,
            "result has no job_token; dropping (possible spoof / legacy worker)"
        );
        write_audit(
            state,
            tenant,
            "result_token_missing",
            Some(&result.execution_id),
            json!({ "subject_tenant": tenant, "body_execution_id": result.execution_id }),
        )
        .await;
        return Ok(());
    }

    // (3) 署名検証（kid で公開鍵を選び verify_strict）。失敗は drop+audit。
    let claims: JobClaims = match state.verifier().verify(&result.job_token) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                execution_id = %result.execution_id,
                subject_tenant = %tenant,
                reason = %e,
                "result token signature verification failed; dropping"
            );
            write_audit(
                state,
                tenant,
                "result_token_verify_failed",
                Some(&result.execution_id),
                json!({ "reason": e.reason(), "subject_tenant": tenant }),
            )
            .await;
            return Ok(());
        }
    };

    // (4a) テナント権威の二重照合: 署名 claim と subject 由来テナントが両方一致すること。
    if claims.tenant_id != tenant {
        tracing::warn!(
            execution_id = %result.execution_id,
            subject_tenant = %tenant,
            claim_tenant = %claims.tenant_id,
            "claim tenant != subject tenant; dropping (possible spoof)"
        );
        write_audit(
            state,
            tenant,
            "result_tenant_mismatch",
            Some(&result.execution_id),
            json!({ "subject_tenant": tenant, "claim_tenant": claims.tenant_id }),
        )
        .await;
        return Ok(());
    }
    // 本文 tenant_id（非空）も subject と一致しなければならない（M3b の本文チェックを保持）。
    if !result.tenant_id.is_empty() && result.tenant_id != tenant {
        tracing::warn!(
            execution_id = %result.execution_id,
            subject_tenant = %tenant,
            body_tenant = %result.tenant_id,
            "result body tenant != subject tenant; dropping"
        );
        write_audit(
            state,
            tenant,
            "result_tenant_mismatch",
            Some(&result.execution_id),
            json!({ "subject_tenant": tenant, "body_tenant": result.tenant_id }),
        )
        .await;
        return Ok(());
    }
    // (4b) claim.execution_id == 本文 execution_id。
    if claims.execution_id != result.execution_id {
        tracing::warn!(
            claim_execution_id = %claims.execution_id,
            body_execution_id = %result.execution_id,
            "claim execution_id != body execution_id; dropping"
        );
        write_audit(
            state,
            tenant,
            "result_token_claim_mismatch",
            Some(&result.execution_id),
            json!({
                "reason": "execution_id_mismatch",
                "claim_execution_id": claims.execution_id,
                "body_execution_id": result.execution_id,
            }),
        )
        .await;
        return Ok(());
    }

    // error は文字列を JSONB に包んで保存 (executions.error は JSONB)。
    let error_json = result.error.as_ref().map(|msg| json!({ "message": msg }));

    // M3b/M3c: finalize は FORCE RLS 下で走る。tx を開いて GUC を設定してから、
    // まず行をひいて claim を権威値と突き合わせ、exp 判定の後に CAS finalize する。
    let mut tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&mut tx, tenant).await?;

    // (4c) 行 SELECT で execution の存在 + version_id / status を取り、claim と照合する。
    let provenance =
        crate::db::find_execution_provenance(&mut *tx, tenant, &result.execution_id).await?;
    let (row_component_id, row_version_id, row_status) = match provenance {
        Some(p) => p,
        None => {
            // 行が無い（subject テナント下に当該 execution が存在しない）→ drop+audit。
            tx.rollback().await?;
            tracing::warn!(
                execution_id = %result.execution_id,
                subject_tenant = %tenant,
                "execution row not found for verified token; dropping"
            );
            write_audit(
                state,
                tenant,
                "result_token_claim_mismatch",
                Some(&result.execution_id),
                json!({ "reason": "unknown_execution" }),
            )
            .await;
            return Ok(());
        }
    };
    if claims.version_id != row_version_id {
        tx.rollback().await?;
        tracing::warn!(
            execution_id = %result.execution_id,
            claim_version_id = %claims.version_id,
            row_version_id = %row_version_id,
            "claim version_id != row version_id; dropping"
        );
        write_audit(
            state,
            tenant,
            "result_token_claim_mismatch",
            Some(&result.execution_id),
            json!({
                "reason": "version_id_mismatch",
                "claim_version_id": claims.version_id,
                "row_version_id": row_version_id,
            }),
        )
        .await;
        return Ok(());
    }

    // (5) exp 判定。失効していても行が pending/running なら受理する（正規の遅延結果）。
    //     失効 + 既に終端なら stale な重複として無視（audit 不要; 良性）。
    let now = chrono::Utc::now().timestamp();
    let row_terminal = matches!(row_status.as_str(), "succeeded" | "failed" | "timeout");
    if now > claims.exp && row_terminal {
        tx.rollback().await?;
        tracing::debug!(
            execution_id = %result.execution_id,
            "expired token and row already terminal; ignoring stale result"
        );
        return Ok(());
    }

    // (6) M5 (§15): worker 自己申告の計量を sanity clamp する。worker は信頼境界外 (§3.3) なので、
    //     version の resource_limits（worker の get_limits と同じ権威値）を引いて上限で切り詰めてから
    //     永続化する（水増し/桁あふれ防止）。usage None（旧 worker / デコード不能 poison）は None のまま渡す。
    //     fail-closed: 権威 limit が引けない（version 行欠落 / resource_limits 破損）ときは、信用できない
    //     worker 値を ResourceLimits::default() で素通しさせず（default は max_fuel=None で fuel が実質
    //     無制限になり水増しを通す fail-open）、リソース指標を記録しない（usage=None）。invocation は
    //     後段の rollup で計上されるため欠落しないが、検証不能なコスト指標で課金しない。
    let clamped_usage = match result.usage {
        Some(raw) => {
            match crate::db::find_version_resource_limits(&mut *tx, tenant, &row_version_id).await?
            {
                Some(limits) => Some(clamp_usage(&raw, &limits)),
                None => {
                    tracing::warn!(
                        execution_id = %result.execution_id,
                        version_id = %row_version_id,
                        "could not resolve authoritative resource_limits; dropping untrusted usage \
                         metrics (fail-closed: invocation still counted, resource cost not billed)"
                    );
                    None
                }
            }
        }
        None => None,
    };

    commit_finalize_and_release(
        state,
        tx,
        tenant,
        &result.execution_id,
        &row_component_id,
        result.status,
        result.output.as_ref(),
        error_json.as_ref(),
        clamped_usage,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn commit_finalize_and_release(
    state: &AppState,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    execution_id: &str,
    component_id: &str,
    status: ExecutionStatus,
    output: Option<&serde_json::Value>,
    error: Option<&serde_json::Value>,
    usage: Option<UsageMetrics>,
) -> anyhow::Result<()> {
    // 戻り値は CAS が遷移させたときだけ `Some(period_start)`（DB の finished_at::date = 単一時計源）。
    let finalized = crate::db::finalize_execution(
        &mut *tx,
        tenant,
        execution_id,
        status,
        output,
        error,
        usage.as_ref(),
    )
    .await?;

    if let Some(outcome) = finalized {
        let usage_for_rollup = usage.unwrap_or_default();
        crate::db::upsert_usage_rollup(
            &mut *tx,
            tenant,
            component_id,
            outcome.period_start,
            status,
            &usage_for_rollup,
        )
        .await?;

        // M10 follow-up (§3.8): CAS が実際に遷移させたときだけ実行時間を observe する
        // （created_at→finished_at の**サーバ時計**。再配送 / 既終端では二重計上しない）。
        state
            .metrics()
            .execution_duration_seconds
            .with_label_values(&[status.as_str()])
            .observe(outcome.duration_secs);
    }

    tx.commit().await?;

    if finalized.is_none() {
        return Ok(());
    }

    state
        .metrics()
        .executions_total
        .with_label_values(&[status.as_str()])
        .inc();

    if let Err(e) = state.store().release_inflight(tenant).await {
        tracing::warn!(
            %execution_id,
            tenant = %tenant,
            error = %e,
            "failed to DECR in-flight counter on finalize; reaper will reconcile"
        );
    }
    Ok(())
}
/// worker 自己申告の計量を `ResourceLimits` 上限で sanity clamp する (M5, §15)。
///
/// worker は署名鍵を持たない**非特権・信頼境界外**ランタイム (§3.3) なので、申告値は無検証だと
/// 課金水増しや桁あふれで rollup の SUM を破壊しうる。各指標を version の resource_limits（worker の
/// `get_limits` と同じ権威値）の上限で頭打ちにしてから永続化する:
/// - `wall_time_ms`  → `max_execution_time_ms`（実行に許される総壁時計上限）
/// - `cpu_fuel_used` → `max_fuel`（fuel 無効化時は上限なし＝`u64::MAX`。worker 側でも 0 化済み）
/// - `peak_memory_bytes` → `max_memory_bytes`（StoreLimits が強制する上限）
/// - `output_bytes`  → `max_memory_bytes`（出力はメモリ上限を超え得ないため同上限で頭打ち）
///
/// 純関数（DB 不要）で clamp ロジックをテスト可能にする。`UsageMetrics` は `Copy`。
fn clamp_usage(raw: &UsageMetrics, limits: &ResourceLimits) -> UsageMetrics {
    UsageMetrics {
        cpu_fuel_used: raw.cpu_fuel_used.min(limits.max_fuel.unwrap_or(u64::MAX)),
        wall_time_ms: raw.wall_time_ms.min(limits.max_execution_time_ms),
        peak_memory_bytes: raw.peak_memory_bytes.min(limits.max_memory_bytes),
        output_bytes: raw.output_bytes.min(limits.max_memory_bytes),
    }
}
/// drop パスで audit_logs に 1 行追記する（§3.7）。
///
/// audit_logs は FORCE RLS + tenant_isolation 下にあるため、専用の短命 tx を開き、
/// 先に `set_tenant_guc(subject tenant)` してから INSERT する（WITH CHECK を通すため
/// tenant_id は subject 由来テナント = drop 時点で唯一権威ある値にする）。
/// audit 自体の失敗で本処理を巻き込まないよう、失敗はログのみ（best-effort）。
async fn write_audit(
    state: &AppState,
    tenant: &str,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    let res: anyhow::Result<()> = async {
        let mut tx = state.pool().begin().await?;
        crate::db::set_tenant_guc(&mut tx, tenant).await?;
        crate::db::insert_audit_log(
            &mut *tx,
            tenant,
            Some("worker"),
            action,
            target,
            Some(&detail),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, action, "failed to write audit_logs row");
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
            output_bytes: 1_000_000,      // > max_memory_bytes 1024
        };
        let c = clamp_usage(&raw, &limits);
        assert_eq!(c.cpu_fuel_used, 1_000);
        assert_eq!(c.wall_time_ms, 500);
        assert_eq!(c.peak_memory_bytes, 1024);
        assert_eq!(c.output_bytes, 1024);
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

    /// fuel 無効化（max_fuel=None）時は cpu_fuel_used に上限が無い（u64::MAX で clamp = 素通し）。
    /// worker 側で既に 0 化済みだが、clamp は申告値をそのまま通す（二次防御として桁あふれは
    /// db 層の saturating_i64 が担保）。
    #[test]
    fn clamp_usage_fuel_disabled_has_no_cpu_clamp() {
        let limits = ResourceLimits {
            max_fuel: None,
            ..test_limits()
        };
        let raw = UsageMetrics {
            cpu_fuel_used: 123_456_789,
            ..Default::default()
        };
        let c = clamp_usage(&raw, &limits);
        assert_eq!(c.cpu_fuel_used, 123_456_789);
    }
}
