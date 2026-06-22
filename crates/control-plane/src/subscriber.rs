//! result + failed (DLQ) subject 購読タスク (M1 → M3b → M4c)。
//!
//! worker が publish する `ResultMessage` を `tenant.*.component.result` で、
//! `FailedMessage` を `tenant.*.component.failed` で受信し、executions を CAS 的に終端状態へ
//! 更新する (db::finalize_execution)。`.failed` 経路は subscriber が常に
//! `ExecutionStatus::Failed` に倒す（DLQ 由来の通知から succeeded には決して至らない, §6.6）。
//!
//! M3b (§3.2 / §3.3):
//!   - テナントは **subject の第 2 トークン**から導出する（`subject_tenant`）。
//!     メッセージ本文の `tenant_id` を盲信しない（spoof 防止）。
//!   - 本文 `tenant_id` が非空かつ subject 由来と不一致なら、なりすまし疑いとして
//!     メッセージを drop する（§3.3 の署名付きトークン検証の前段）。
//!   - finalize_execution は FORCE RLS 下で走るため（migrations/0004_rls.sql）、
//!     tx を開いて先に set_tenant_guc(subject_tenant) を設定してから実行する。
//!
//! M3c (§3.3「結果の出所認証」): worker が echo した署名トークン `job_token` を
//! ここで検証する。署名（kid で選んだ公開鍵）+ claim 突き合わせ（execution_id /
//! tenant_id / version_id を execution 行と subject 由来テナントに照合）を通った
//! ものだけを CAS finalize する。検証失敗は **drop して audit_logs に 1 行追記** する
//! （改竄・なりすましの監査証跡, §3.7）。署名 claim が新たな権威であり、M3b の
//! subject 由来テナントと **両方一致** することを要求する（M3b より強い）。
//! exp は「失効しても execution 行が pending/running なら受理」する（正規の遅延結果を
//! 取りこぼさない, §3.3）。
//!
//! M4c (§6.6 MUST): `.failed` (DLQ) subscriber を追加した。`.result` も `.failed` も来ない
//! 「無音失踪」は reaper の stuck-execution sweeper（reaper.rs）が拾う。両者を合わせて、
//! 終端化 + in-flight DECR の漏れ経路を塞ぐ（カウンタリーク → 永続 429 のテナント自己 DoS を防止）。
//! result / failed は core NATS subscribe（非 JetStream）でブラスト半径を最小にとどめる:
//! durable stream 化は別スライスで段階導入する（§6.6 follow-up）。
//!
//! 検証 + CAS finalize + DECR の **コア手順は両 subject で共通**（[`finalize_verified`]）。
//! 異なるのは (a) どこから execution_id / job_token / 終端 status / error を取り出すか、
//! (b) audit のアクション名（`result_*` か `dlq_*`）、(c) DLQ は必ず `Failed` に倒す（本文に
//! 状態フィールドが無いため、succeeded の取り違えがそもそも起こらない）、の 3 点だけである。

use futures::StreamExt;
use serde_json::json;

use faas_shared::{
    failed_subject_wildcard, result_subject_wildcard, tenant_from_subject, ExecutionStatus,
    FailedMessage, JobClaims, ResultMessage,
};

use crate::state::AppState;

/// result subject を購読し続けるバックグラウンドループ。
///
/// `main` から `tokio::spawn` される。エラーは握りつぶさずログに出す
/// (M1 は購読断で panic させず、ループ継続/再接続は NATS クライアント任せ)。
pub async fn run(state: AppState) {
    // §3.3: テナントワイルドカードで購読し、テナントは実際の subject から導出する。
    let subject = result_subject_wildcard();

    let mut subscription = match state.nats().subscribe(subject.to_string()).await {
        Ok(sub) => sub,
        Err(e) => {
            tracing::error!(error = %e, subject = %subject, "failed to subscribe to result subject");
            return;
        }
    };

    tracing::info!(subject = %subject, "subscribed to result subject (tenant wildcard)");

    while let Some(msg) = subscription.next().await {
        // テナントは subject から導出する（本文を盲信しない, §3.3）。
        let subject_tenant = match tenant_from_subject(msg.subject.as_str()) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    subject = %msg.subject,
                    "result message on unexpected subject shape; dropping"
                );
                continue;
            }
        };

        if let Err(e) = handle_message(&state, subject_tenant, &msg.payload).await {
            tracing::error!(error = %e, "failed to process result message");
        }
    }

    tracing::warn!(subject = %subject, "result subscription ended");
}

/// failed (DLQ) subject 購読のバックグラウンドループ (M4c, §6.6 MUST)。
///
/// `tenant.*.component.failed` を受け、`.result` も `.failed` も来ない無音失踪を排除する。
/// 構造は [`run`] と対称: subject 由来テナントを唯一権威として `subject_tenant` を抽出し、
/// 個別メッセージ処理は [`handle_failed_message`] に委譲する。`main` が独立に `tokio::spawn` する
/// （結果経路と DLQ 経路は互いに blocking しない）。
pub async fn run_failed(state: AppState) {
    let subject = failed_subject_wildcard();

    let mut subscription = match state.nats().subscribe(subject.to_string()).await {
        Ok(sub) => sub,
        Err(e) => {
            tracing::error!(error = %e, subject = %subject, "failed to subscribe to DLQ subject");
            return;
        }
    };

    tracing::info!(subject = %subject, "subscribed to failed (DLQ) subject (tenant wildcard)");

    while let Some(msg) = subscription.next().await {
        let subject_tenant = match tenant_from_subject(msg.subject.as_str()) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    subject = %msg.subject,
                    "DLQ message on unexpected subject shape; dropping"
                );
                continue;
            }
        };

        if let Err(e) = handle_failed_message(&state, subject_tenant, &msg.payload).await {
            tracing::error!(error = %e, "failed to process DLQ message");
        }
    }

    tracing::warn!(subject = %subject, "DLQ subscription ended");
}

/// subject 由来の `tenant`（M3b の権威）と署名 claim（M3c の権威）で result を反映する。
///
/// 検証順 (§3.3):
/// 1. 終端状態か（非終端は無視）。
/// 2. job_token 在否（空は drop+audit: token_missing）。
/// 3. 署名検証（kid で公開鍵を選び verify_strict。失敗は drop+audit: verify_failed）。
/// 4. claim 突き合わせ（claim.tenant_id == subject tenant、本文 tenant_id（非空）== subject、
///    claim.execution_id == 本文 execution_id。行 SELECT して version_id/status も照合）。
/// 5. exp 判定（失効していても行が pending/running なら受理。終端済みなら stale として無視）。
/// 6. CAS finalize。
///
/// M4a (§3.8): ここを `subscriber.handle_message` span で包み、本文を読んだ時点で `execution_id` /
/// `tenant_id` を span フィールドに記録する。以降のすべてのログは自動でこのコリレーション ID を持つ。
#[tracing::instrument(
    name = "subscriber.handle_message",
    skip_all,
    fields(execution_id = tracing::field::Empty, tenant_id = %tenant)
)]
async fn handle_message(state: &AppState, tenant: &str, payload: &[u8]) -> anyhow::Result<()> {
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
    let (row_version_id, row_status) = match provenance {
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

    // (6) CAS finalize（status NOT IN terminal のときだけ遷移する）。共通ロジックは
    //     `commit_finalize_and_release` に集約する（DLQ 経路と同一の DECR + audit 規則を共有）。
    commit_finalize_and_release(
        state,
        tx,
        tenant,
        &result.execution_id,
        result.status,
        result.output.as_ref(),
        error_json.as_ref(),
        FinalizeOrigin::Result,
    )
    .await
}

/// `.failed` (DLQ) 1 通分のメッセージ処理 (M4c, §6.6 MUST)。
///
/// 手順は [`handle_message`] と対称だが、本文には `status` フィールドが無いため subscriber は
/// **常に [`ExecutionStatus::Failed`]** に倒す（DLQ 経路から succeeded への遷移はスキーマ上不可能）。
/// 結果メッセージとは独立した audit アクション（`dlq_*`）で経路の区別を保ち、postmortem で
/// 「どの leg が終端化したか」が必ず判別できるようにする（reaper の `stuck_finalized` も別経路）。
///
/// 期待される `reason` 文字列（worker 側で記録）:
/// - `"max_deliver exhausted"` — 最終再配送の publish 失敗で worker が DLQ に逃がした。
/// - `"worker panic"` / 自由文字列 — 任意。subscriber は executions.error に `{"message": reason}`
///   で保存するため、運用者は execution 行から DLQ 由来の理由が読める。
#[tracing::instrument(
    name = "subscriber.handle_failed_message",
    skip_all,
    fields(execution_id = tracing::field::Empty, tenant_id = %tenant)
)]
async fn handle_failed_message(
    state: &AppState,
    tenant: &str,
    payload: &[u8],
) -> anyhow::Result<()> {
    let failed: FailedMessage = serde_json::from_slice(payload)?;
    tracing::Span::current().record("execution_id", failed.execution_id.as_str());

    // (1) job_token 在否。空（旧 worker / 欠落）は fail-closed で drop+audit。
    if failed.job_token.is_empty() {
        tracing::warn!(
            execution_id = %failed.execution_id,
            subject_tenant = %tenant,
            "DLQ has no job_token; dropping (possible spoof / legacy worker)"
        );
        write_audit(
            state,
            tenant,
            "dlq_token_missing",
            Some(&failed.execution_id),
            json!({ "subject_tenant": tenant, "body_execution_id": failed.execution_id }),
        )
        .await;
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["dropped"])
            .inc();
        return Ok(());
    }

    // (2) 署名検証（result subscriber と同じ verify_strict / kid 経路）。失敗は drop+audit。
    let claims: JobClaims = match state.verifier().verify(&failed.job_token) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                execution_id = %failed.execution_id,
                subject_tenant = %tenant,
                reason = %e,
                "DLQ token signature verification failed; dropping"
            );
            write_audit(
                state,
                tenant,
                "dlq_token_verify_failed",
                Some(&failed.execution_id),
                json!({ "reason": e.reason(), "subject_tenant": tenant }),
            )
            .await;
            state
                .metrics()
                .dlq_finalized_total
                .with_label_values(&["dropped"])
                .inc();
            return Ok(());
        }
    };

    // (3) claim/subject/body テナント・execution_id の三重照合（result 経路と同規約）。
    if claims.tenant_id != tenant {
        tracing::warn!(
            execution_id = %failed.execution_id,
            subject_tenant = %tenant,
            claim_tenant = %claims.tenant_id,
            "DLQ claim tenant != subject tenant; dropping (possible spoof)"
        );
        write_audit(
            state,
            tenant,
            "dlq_tenant_mismatch",
            Some(&failed.execution_id),
            json!({ "subject_tenant": tenant, "claim_tenant": claims.tenant_id }),
        )
        .await;
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["dropped"])
            .inc();
        return Ok(());
    }
    if !failed.tenant_id.is_empty() && failed.tenant_id != tenant {
        tracing::warn!(
            execution_id = %failed.execution_id,
            subject_tenant = %tenant,
            body_tenant = %failed.tenant_id,
            "DLQ body tenant != subject tenant; dropping"
        );
        write_audit(
            state,
            tenant,
            "dlq_tenant_mismatch",
            Some(&failed.execution_id),
            json!({ "subject_tenant": tenant, "body_tenant": failed.tenant_id }),
        )
        .await;
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["dropped"])
            .inc();
        return Ok(());
    }
    if claims.execution_id != failed.execution_id {
        tracing::warn!(
            claim_execution_id = %claims.execution_id,
            body_execution_id = %failed.execution_id,
            "DLQ claim execution_id != body execution_id; dropping"
        );
        write_audit(
            state,
            tenant,
            "dlq_token_claim_mismatch",
            Some(&failed.execution_id),
            json!({
                "reason": "execution_id_mismatch",
                "claim_execution_id": claims.execution_id,
                "body_execution_id": failed.execution_id,
            }),
        )
        .await;
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["dropped"])
            .inc();
        return Ok(());
    }

    // (4) 行 SELECT で execution の存在 + version_id を引いて claim と照合。
    let mut tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&mut tx, tenant).await?;
    let provenance =
        crate::db::find_execution_provenance(&mut *tx, tenant, &failed.execution_id).await?;
    let (row_version_id, row_status) = match provenance {
        Some(p) => p,
        None => {
            tx.rollback().await?;
            tracing::warn!(
                execution_id = %failed.execution_id,
                subject_tenant = %tenant,
                "DLQ execution row not found for verified token; dropping"
            );
            write_audit(
                state,
                tenant,
                "dlq_token_claim_mismatch",
                Some(&failed.execution_id),
                json!({ "reason": "unknown_execution" }),
            )
            .await;
            state
                .metrics()
                .dlq_finalized_total
                .with_label_values(&["dropped"])
                .inc();
            return Ok(());
        }
    };
    if claims.version_id != row_version_id {
        tx.rollback().await?;
        tracing::warn!(
            execution_id = %failed.execution_id,
            claim_version_id = %claims.version_id,
            row_version_id = %row_version_id,
            "DLQ claim version_id != row version_id; dropping"
        );
        write_audit(
            state,
            tenant,
            "dlq_token_claim_mismatch",
            Some(&failed.execution_id),
            json!({
                "reason": "version_id_mismatch",
                "claim_version_id": claims.version_id,
                "row_version_id": row_version_id,
            }),
        )
        .await;
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["dropped"])
            .inc();
        return Ok(());
    }

    // (5) exp 判定（result 経路と同規約）。失効 + 既終端は良性 stale として無音 drop。
    let now = chrono::Utc::now().timestamp();
    let row_terminal = matches!(row_status.as_str(), "succeeded" | "failed" | "timeout");
    if now > claims.exp && row_terminal {
        tx.rollback().await?;
        tracing::debug!(
            execution_id = %failed.execution_id,
            "expired DLQ token and row already terminal; ignoring stale DLQ message"
        );
        state
            .metrics()
            .dlq_finalized_total
            .with_label_values(&["stale"])
            .inc();
        return Ok(());
    }

    // (6) CAS finalize は **常に Failed**（DLQ 経路の終端は succeeded を絶対に通さない, §6.6）。
    //     error には worker が記録した reason を `{"message": reason}` で保存し、運用者が DLQ
    //     由来の失敗理由を execution 行から読めるようにする。
    let error_json = json!({ "message": failed.reason });
    commit_finalize_and_release(
        state,
        tx,
        tenant,
        &failed.execution_id,
        ExecutionStatus::Failed,
        None,
        Some(&error_json),
        FinalizeOrigin::Dlq,
    )
    .await
}

/// CAS finalize + audit + in-flight DECR の共通コア（result / DLQ 両経路で共有）。
///
/// `tx` は呼び出し側で `set_tenant_guc(tenant)` 済み・claim 照合通過後に渡される。本関数で commit し、
/// `updated > 0` のときだけメトリクス inc + `release_inflight` を行う（重複配送では二重 DECR しない）。
/// `release_inflight` の失敗はログのみ（reaper の DB COUNT 再同期が補正する）。
#[allow(clippy::too_many_arguments)]
async fn commit_finalize_and_release(
    state: &AppState,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    execution_id: &str,
    status: ExecutionStatus,
    output: Option<&serde_json::Value>,
    error: Option<&serde_json::Value>,
    origin: FinalizeOrigin,
) -> anyhow::Result<()> {
    let updated =
        crate::db::finalize_execution(&mut *tx, tenant, execution_id, status, output, error)
            .await?;
    tx.commit().await?;

    if updated == 0 {
        // 既終端で no-op。result/DLQ で再配送と CP 双方を回しても二重 DECR しない（CAS が冪等性の境界）。
        match origin {
            FinalizeOrigin::Result => {
                tracing::debug!(
                    %execution_id,
                    "result already finalized or unknown; ignoring"
                );
            }
            FinalizeOrigin::Dlq => {
                tracing::debug!(
                    %execution_id,
                    "DLQ message arrived after execution already finalized; ignoring (stale)"
                );
                state
                    .metrics()
                    .dlq_finalized_total
                    .with_label_values(&["stale"])
                    .inc();
            }
        }
        return Ok(());
    }

    // M4a (§3.8): 終端化件数を観測する。subscriber は **正規の終端 writer**（reaper の
    // stuck-sweeper を除く）。`executions_total` は result/DLQ/sweeper すべてで同名カウンタに
    // 合流させ、合算で「失敗合計」が取れるようにする（経路区別は別カウンタ）。
    state
        .metrics()
        .executions_total
        .with_label_values(&[status.as_str()])
        .inc();

    match origin {
        FinalizeOrigin::Result => {
            tracing::info!(
                %execution_id,
                status = %status,
                "execution finalized (token verified)"
            );
        }
        FinalizeOrigin::Dlq => {
            // M4c: DLQ 経由の正規 finalize は dlq_finalized_total{outcome="finalized"} に計上する。
            // executions_total{status="failed"} と二重計上にはしない（dlq_* は経路の判別軸）。
            state
                .metrics()
                .dlq_finalized_total
                .with_label_values(&["finalized"])
                .inc();
            tracing::warn!(
                %execution_id,
                "execution finalized to 'failed' via DLQ (.failed) subject (token verified)"
            );
        }
    }

    // M3d (§8): in-flight 同時実行カウンタを DECR する。subscriber は **正規の終端 writer**
    // であり、CAS が実際に遷移させた（updated == 1）ときだけ DECR する。これにより重複配送
    // （updated == 0）では二重 DECR しない。result/DLQ の双方で同一契約。
    if let Err(e) = state.store().release_inflight(tenant).await {
        tracing::warn!(
            %execution_id,
            tenant = %tenant,
            error = %e,
            origin = ?origin,
            "failed to DECR in-flight counter on finalize; reaper will reconcile"
        );
    }
    Ok(())
}

/// finalize 経路の出所（観測 + audit 区別のため）。
#[derive(Debug, Clone, Copy)]
enum FinalizeOrigin {
    /// `.result` 由来（worker の通常終端通知）。
    Result,
    /// `.failed` (DLQ) 由来（worker の最終配送失敗・無音失踪救済）。
    Dlq,
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

// ============================================================================
// Unit tests (DB-free): CAS-based DLQ idempotency と DECR 契約のモデル検査。
//
// `commit_finalize_and_release` の本物は DB Transaction を要求するため、ここでは
// その契約 ——「`updated == 1` のときだけ DECR + executions_total を inc し、`updated == 0` の
// 重複配送では DECR せず `dlq_finalized_total{outcome="stale"}` だけを inc する」—— を、
// 同等のロジックを持つ純関数（[`apply_dlq_finalize_outcome`]）に対して in-proc store +
// metrics スパイで検証する。これにより:
// - DLQ 再配送に対して二重 DECR しないこと（カウンタリーク防止 §6.6）
// - finalize 成功・stale・dropped の 3 outcome が `dlq_finalized_total` のラベルへ
//   正しく分岐すること
// が live DB なしで担保できる。本物の `commit_finalize_and_release` を変更したら、
// この純関数も同期して更新すること（テストが「契約」のミラーになる）。
// ============================================================================

#[cfg(test)]
mod tests {
    use crate::metrics::Metrics;
    use crate::store::{InProcStore, InflightParams, Store};

    /// `commit_finalize_and_release` の契約をミラーした純関数（DB-free モデル）:
    /// `updated` 行数に応じて DLQ outcome ラベルを選び、in-flight DECR を行う。
    ///
    /// - `updated == 1`: 終端遷移を起こした正規 finalize。`dlq_finalized_total{finalized}` を inc
    ///   して store の DECR を 1 回呼ぶ（subscriber は終端 writer なので必ず 1 つ slot を解放する）。
    /// - `updated == 0`: 既終端のため CAS が no-op だった。`dlq_finalized_total{stale}` を inc
    ///   するが、**DECR は呼ばない**（pending->terminal の遷移は既に過去に DECR 済み or sweeper 処理済み）。
    async fn apply_dlq_finalize_outcome(
        metrics: &Metrics,
        store: &dyn Store,
        tenant: &str,
        updated: u64,
    ) {
        if updated == 0 {
            metrics
                .dlq_finalized_total
                .with_label_values(&["stale"])
                .inc();
            return;
        }
        metrics
            .dlq_finalized_total
            .with_label_values(&["finalized"])
            .inc();
        metrics
            .executions_total
            .with_label_values(&["failed"])
            .inc();
        let _ = store.release_inflight(tenant).await;
    }

    /// M4c (§6.6) DLQ 冪等性: 同一 execution に対して finalize=1 が 1 度起き、その後の
    /// 再配送は CAS で `updated == 0` になる。slot は 1 度だけ DECR され、二重 DECR は起きない。
    ///
    /// shape: 1 件予約 → 1 度の finalize で 0 → 再配送 2 回（updated==0）は DECR せず 0 に張り付く。
    #[tokio::test]
    async fn dlq_finalize_decrements_inflight_only_once() {
        let metrics = Metrics::init();
        let store = InProcStore::new();
        let tenant = "ten_a";

        // pending を 1 つ予約（subscriber が DECR する対象）。
        let p = InflightParams {
            max: 10,
            ttl_secs: 60,
        };
        let d = store.reserve_inflight(tenant, p).await.unwrap();
        assert!(d.admitted);
        assert_eq!(store.peek_inflight(tenant), 1);

        // 最初の DLQ 到着: CAS 成功（updated==1）→ DECR が 1 回走り 0 になる。
        apply_dlq_finalize_outcome(&metrics, &store, tenant, 1).await;
        assert_eq!(store.peek_inflight(tenant), 0);

        // 再配送（同じ DLQ message が max_deliver で再送）: CAS no-op（updated==0）。
        // ここで DECR が再度走ると -1 までさらに減ってカウンタリーク（永続 429 のテナント自己 DoS）が起きる。
        // 本契約はそれを禁じる。
        for _ in 0..2 {
            apply_dlq_finalize_outcome(&metrics, &store, tenant, 0).await;
            assert_eq!(
                store.peek_inflight(tenant),
                0,
                "stale DLQ deliveries must not DECR (would cause counter leak / runaway 429)"
            );
        }
    }

    /// M4c metrics: 正規 finalize は `dlq_finalized_total{outcome="finalized"}` を 1 度だけ
    /// inc し、再配送は `outcome="stale"` を inc する（運用が DLQ 遅延と無音失踪を分離できる）。
    #[tokio::test]
    async fn dlq_finalize_metrics_labels_distinguish_finalized_and_stale() {
        let metrics = Metrics::init();
        let store = InProcStore::new();
        let tenant = "ten_a";

        // 予約しておく（DECR 経路を踏ませるため）。
        store
            .reserve_inflight(
                tenant,
                InflightParams {
                    max: 10,
                    ttl_secs: 60,
                },
            )
            .await
            .unwrap();

        apply_dlq_finalize_outcome(&metrics, &store, tenant, 1).await; // finalized
        apply_dlq_finalize_outcome(&metrics, &store, tenant, 0).await; // stale (redelivery)
        apply_dlq_finalize_outcome(&metrics, &store, tenant, 0).await; // stale

        let finalized = metrics
            .dlq_finalized_total
            .with_label_values(&["finalized"])
            .get();
        let stale = metrics
            .dlq_finalized_total
            .with_label_values(&["stale"])
            .get();
        let failed = metrics
            .executions_total
            .with_label_values(&["failed"])
            .get();
        assert_eq!(finalized, 1, "exactly one CAS transition → finalized");
        assert_eq!(stale, 2, "redeliveries → stale (not finalized)");
        assert_eq!(
            failed, 1,
            "executions_total must move only on real finalize"
        );
    }
}
