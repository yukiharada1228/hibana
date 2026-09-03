//! enqueue ヘルパ (M6-0, control-plane の心臓部, §15)。
//!
//! M6 の全 non-HTTP 入口（同期 invoke / Cron / 外部イベント / chain）が、HTTP invoke が既に
//! 通っている「pending INSERT(provenance+冪等列) → tx commit → job_token 署名 → JetStream
//! publish(Nats-Msg-Id=execution_id)」という **単一の正規パス** へ合流するための再利用可能な
//! コアをここに閉じる。新しい入口は「いつ・誰が enqueue するか」だけが違い、冪等性・provenance・
//! 計量・テナント分離の不変条件は本ヘルパ（＝既存パス）がそのまま担保する。
//!
//! 抽出方針（不変条件を 1 行も変えない, 設計書 §2）:
//! - pending INSERT は `db::insert_pending_execution_with_provenance` をそのまま使う
//!   （savepoint で 23505 を捕捉 → 再 SELECT → 同一 body 既存返却 / 異 body は呼び出し側が 409 判定）。
//! - job_token 署名は `state.signer().sign(&claims)` を完全に流用（claims は version_id/kid/iat/exp）。
//! - publish は `Nats-Msg-Id=execution_id` 付き JetStream publish を流用（冪等性 layer 3）。
//! - `reply_to`（M6a 同期 invoke）/ `origin`（観測・監査）を JobMessage に載せるのが唯一の新規。
//!
//! admission（rate-limit / in-flight reserve）と input_ref の **完全一致検証**・presign は呼び出し側
//! （HTTP / Cron / event）の責務として残す。本ヘルパは「確定済みの enqueue」だけを担う。

use async_nats::HeaderMap as NatsHeaderMap;
use faas_shared::{invoke_subject, ExecutionStatus, JobClaims, JobMessage};
use serde_json::Value;

use crate::db;
use crate::error::AppError;
use crate::state::AppState;

/// enqueue 1 件分の確定済みリクエスト (M6-0)。
///
/// `wasm_url` / `input_url` は呼び出し側が presign 済み（HTTP は input_ref 完全一致検証を通した
/// キーのみを presign する, §3.4）。`execution_id` は呼び出し側が確定済み（HTTP の新規採番 /
/// 予約済み id、Cron/event の新規採番）。`idempotency_key` は HTTP の Idempotency-Key、または
/// non-HTTP 起点の安定キー（`cron_idempotency_key` / `event_idempotency_key`）。
pub struct EnqueueRequest<'a> {
    /// 権威テナント（GUC と一致する。本文盲信ではなく呼び出し側が principal / subject 由来で確定）。
    pub tenant: &'a str,
    /// Component 名（JobMessage の観測・解決用）。
    pub component_name: &'a str,
    /// 解決済み component の id（executions.component_id）。
    pub component_id: &'a str,
    /// active version の id（executions.version_id / job_token claim の version_id）。
    pub version_id: &'a str,
    /// active version の semver（JobMessage の観測用）。
    pub version: &'a str,
    /// 本体の sha256（worker のキャッシュキー, §3.6）。
    pub wasm_sha256: &'a str,
    /// 本体への短命 presigned GET URL（呼び出し側が発行）。
    pub wasm_url: String,
    /// 大入力の退避オブジェクトへの、そのキー限定の短命 presigned GET URL（インラインは None）。
    pub input_url: Option<String>,
    /// インライン入力（大入力時は worker が input_url を優先する）。
    pub input: &'a Value,
    /// 大入力の退避参照（executions.input_ref へ保存。インラインは None）。
    pub input_ref: Option<&'a str>,
    /// 確定済み execution_id。
    pub execution_id: String,
    /// 冪等キー（layer 1, `executions(tenant_id, idempotency_key)` UNIQUE）。
    pub idempotency_key: Option<&'a str>,
    /// 冪等 body hash（同一キー + 異 body を 409 判定するための正準 hash）。
    pub request_hash: Option<&'a str>,
    /// job_token claim の iat（unix 秒）。呼び出し側で確定（HTTP/non-HTTP 共通の Signer 流用）。
    pub iat: i64,
    /// job_token claim の exp（unix 秒）。`iat + token_exp_offset_secs(max_wall_time_ms)`。
    pub exp: i64,
    /// M6a: 同期 invoke の Core NATS reply 先。M6-0 では常に None（M6a で consume）。
    pub reply_to: Option<String>,
    /// M6: 起動由来 "http_invoke" / "cron" / "event" / "chain"（観測・監査用）。
    pub origin: &'static str,
    /// M6c: Component チェーンのホップ深さ。root 起動（HTTP/Cron/Object Storage）は 0、chain 下流のみ
    /// 上流深さ+1。`MAX_CHAIN_DEPTH` 超過の起動は呼び出し側（chain フック）で拒否する（循環暴走防止）。
    pub chain_depth: i32,
    /// M7a: version 決定理由（`"stable"` | `"canary"`）。`executions.routing_reason` へ保存し、
    /// **publish ack 成功後**の `faas_canary_routed_total` にも使う。
    /// 値は `resolve_version_for_enqueue` が返した `RoutedVersion::reason` をそのまま渡すこと
    /// （呼び出し側で組み立てない ＝ 解決と記録の食い違いを構造的に防ぐ）。
    pub routing_reason: &'static str,
}

/// 選ばれた版と、その選択理由 (M7a)。
pub struct RoutedVersion {
    /// 解決された component の id（executions.component_id / routing バケットのドメイン）。
    pub component_id: String,
    /// 実際に起動する版（stable か canary のどちらか）。
    pub selected: db::ActiveVersion,
    /// `routing::RoutingReason::as_str()`（`"stable"` | `"canary"`）。
    pub reason: &'static str,
}

/// 全入口（HTTP invoke / Cron / event / chain）が通る**唯一のバージョン解決点** (M7a, §6.7 / §15)。
///
/// `enqueue.rs` に置くのは意図的である。本モジュールは M6-0 で「全入口が合流する唯一の正規パス」
/// として作られており、解決をここに閉じれば **どの入口も canary をバイパスできない**。
///
/// `tx` は `set_tenant_guc` 済み（FORCE RLS 下）。active version が無い / stable ポインタが壊れて
/// いるときは `None`（呼び出し側が従来どおり 400 / cron の advance-only skip / delivery-only skip
/// に倒す）。
///
/// **メトリクスはここで inc しない**。解決しても enqueue されない経路が複数あるため
/// （HTTP は admission reserve の 429・presign 失敗・publish backpressure がこの後に来る。cron は
/// 冪等ヒットで skip する）。計上は `enqueue_execution` が `Enqueued` を返す直前に行う（§2.9）。
pub async fn resolve_version_for_enqueue(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    component_name: &str,
    routing_key: &str,
) -> Result<Option<RoutedVersion>, sqlx::Error> {
    let Some(routing) = db::resolve_component_routing(&mut *tx, tenant, component_name).await?
    else {
        return Ok(None);
    };
    let bucket = crate::routing::routing_bucket(&routing.component_id, routing_key);
    let (selected, reason) = crate::routing::select_version(&routing, bucket);
    Ok(Some(RoutedVersion {
        component_id: routing.component_id.clone(),
        selected: selected.clone(),
        reason: reason.as_str(),
    }))
}

/// enqueue 失敗の分類 (M6-0)。
///
/// publish 失敗（JetStream への書き込み TimedOut/BrokenPipe 等）は **下流が詰まっている**シグナルで
/// あり、HTTP invoke は 500 ではなく 429（バックオフ後リトライ）へ写像したい（§8 MUST: キュー滞留
/// 時もバックプレッシャとして 429）。一方 INSERT/commit など DB 層の失敗は通常の `AppError`（500 等）
/// へ写像する。pending 行は publish 失敗時点で既に commit 済み（孤児だが reaper の stuck-execution
/// sweeper が deadline で failed に倒す = 二段救済）。
pub enum EnqueueError {
    /// JetStream publish / ack 失敗（バックプレッシャ）。HTTP は 429 + Retry-After へ写像する。
    PublishBackpressure,
    /// その他（DB 層など）。通常の `AppError` 写像。
    Other(AppError),
}

impl From<AppError> for EnqueueError {
    fn from(e: AppError) -> Self {
        EnqueueError::Other(e)
    }
}

impl From<sqlx::Error> for EnqueueError {
    fn from(e: sqlx::Error) -> Self {
        EnqueueError::Other(e.into())
    }
}

impl From<serde_json::Error> for EnqueueError {
    fn from(e: serde_json::Error) -> Self {
        EnqueueError::Other(faas_shared::FaasError::Serialization(e).into())
    }
}

/// enqueue の結果 (M6-0)。
pub enum EnqueueOutcome {
    /// 新規に pending を確定し JobMessage を publish した。
    Enqueued { execution_id: String },
    /// 冪等ヒット（同一 (tenant, idempotency_key) の既存行が在った）。`status` は既存行の現状態。
    /// 「同一 body → 既存返却 / 異 body → 409」の最終判定（body hash 照合）は呼び出し側が行う
    /// （HTTP は audit 追記 + 409、non-HTTP は二重発火吸収として黙って skip するなど方針が異なる）。
    IdempotentHit {
        execution_id: String,
        status: ExecutionStatus,
        /// 既存行に保存されていた冪等 body hash（NULL の理論行に備え Option）。
        stored_request_hash: Option<String>,
    },
}

/// 「pending INSERT(provenance+冪等列) → tx commit → job_token 署名 → JobMessage publish
/// (Nats-Msg-Id=execution_id, reply_to/origin 同梱)」を 1 関数に閉じる (M6-0, §15)。
///
/// `tx` は **set_tenant_guc(req.tenant) 済み**（FORCE RLS 下で INSERT が通る前提）。INSERT は
/// savepoint（sqlx の nested begin = SAVEPOINT）で包み、23505（部分 UNIQUE 違反）を捕捉したら
/// savepoint を rollback して abort 部分状態を解消してから外側 tx で再 SELECT し、`IdempotentHit`
/// を返す（既存行が引けなければ 23505 を素のエラーとして上げる）。INSERT 成功時は tx を commit
/// してから署名・publish する（NATS publish をトランザクション境界の外に出す: publish 中に
/// tx/接続を保持しない）。
///
/// 注: 本ヘルパは admission スロットの release を行わない（reserve したか否かは呼び出し側の文脈で
/// あり、HTTP は `IdempotentHit` / publish 失敗時の release を自分の文脈で管理する）。publish 失敗時の
/// バックプレッシャ写像（429）も呼び出し側の HTTP 文脈に閉じるため、ここは `AppError` を返すに留める。
pub async fn enqueue_execution(
    state: &AppState,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    req: EnqueueRequest<'_>,
) -> Result<EnqueueOutcome, EnqueueError> {
    use sqlx::Acquire as _;

    // --- 1) pending INSERT（savepoint で 23505 を捕捉） ---
    let insert_result = {
        let mut sp = tx.begin().await?;
        let r = db::insert_pending_execution_with_provenance(
            &mut *sp,
            &req.execution_id,
            req.tenant,
            req.component_id,
            req.version_id,
            req.input,
            req.idempotency_key,
            req.request_hash,
            Some(state.signer().kid()),
            req.input_ref,
            req.chain_depth,
            req.routing_reason,
        )
        .await;
        match r {
            Ok(()) => {
                sp.commit().await?;
                Ok(())
            }
            Err(e) => {
                sp.rollback().await?;
                Err(e)
            }
        }
    };

    if let Err(e) = insert_result {
        // 23505 = 同一 (tenant, idempotency_key) の既存行。再 SELECT して IdempotentHit を返す。
        if is_unique_violation(&e) {
            if let Some(key) = req.idempotency_key {
                if let Some((existing_id, existing_status, stored_hash)) =
                    db::find_execution_by_idempotency_key(&mut *tx, req.tenant, key).await?
                {
                    // 冪等ヒットは新規 publish しない。tx は読み取りのみで終わるので commit して解放する。
                    tx.commit().await?;
                    let status = parse_execution_status(&existing_status)
                        .unwrap_or(ExecutionStatus::Pending);
                    return Ok(EnqueueOutcome::IdempotentHit {
                        execution_id: existing_id,
                        status,
                        stored_request_hash: stored_hash,
                    });
                }
            }
        }
        return Err(EnqueueError::Other(e.into()));
    }

    // --- 1.5) M7c (§4.6): env-token の要否を判定する ---
    // 「当該 component に生存する secret が 1 件以上あるか」だけを見る。**secret 名も件数も
    // wire に載せない**（`env_token` が `Some` かどうかがそのままシグナルになる）。
    // secret を 1 つも持たない component では worker の HTTP 往復がゼロになる（pay-per-use）。
    // GUC 済み tx の中で引く必要があるので commit より前に行う。
    let needs_env_token =
        db::component_has_live_secrets(&mut *tx, req.tenant, req.component_id).await?;

    // --- 2) tx を確定してから署名・publish する（publish をトランザクション境界の外へ） ---
    tx.commit().await?;

    // --- 3) job_token を mint（HTTP と同一の Signer::sign。provenance §3.3） ---
    let claims = JobClaims {
        execution_id: req.execution_id.clone(),
        tenant_id: req.tenant.to_string(),
        version_id: req.version_id.to_string(),
        kid: state.signer().kid().to_string(),
        iat: req.iat,
        exp: req.exp,
    };
    let job_token = state.signer().sign(&claims);

    // --- 3.5) M7c (§4.6): 必要なときだけ env-token を mint する ---
    // **job_token を流用しない**。job_token は ResultMessage / FailedMessage にも verbatim に
    // echo されるため、流用すると result / DLQ / reply の購読権しか持たない主体が引き換え
    // トークンを得て secret を読めてしまう。専用 claim（別ドメインタグ + aud）にする。
    let env_token = needs_env_token.then(|| {
        state.signer().sign_env(&faas_shared::EnvClaims {
            execution_id: req.execution_id.clone(),
            tenant_id: req.tenant.to_string(),
            version_id: req.version_id.to_string(),
            component_id: req.component_id.to_string(),
            aud: faas_shared::ENV_TOKEN_AUDIENCE.to_string(),
            kid: state.signer().kid().to_string(),
            iat: req.iat,
            // job_token と同じ TTL 式（再配送を含む最悪滞留 + 実行上限 + 余裕）で有限。
            exp: req.exp,
        })
    });

    // --- 4) JobMessage を invoke_subject へ publish（Nats-Msg-Id=execution_id, 冪等 layer 3） ---
    let job = JobMessage {
        execution_id: req.execution_id.clone(),
        tenant_id: req.tenant.to_string(),
        component: req.component_name.to_string(),
        version: req.version.to_string(),
        wasm_sha256: req.wasm_sha256.to_string(),
        wasm_url: req.wasm_url,
        input: req.input.clone(),
        input_url: req.input_url,
        job_token,
        // M6a: 同期 invoke の reply 先。M6-0 では常に None。
        reply_to: req.reply_to,
        // M6: 起動由来（http_invoke / cron / event / chain）。
        origin: Some(req.origin.to_string()),
        // M7c: secret を持つ component のときだけ載る引き換えトークン。
        env_token,
    };
    let payload = serde_json::to_vec(&job)?;

    // --- M8-3 (§3.7.3): publish は必ず lane 作成の後 ---
    // これで「メッセージは stream にあるが誰も購読していない」窓が消える。
    // `TENANT_LANES_ENABLED=false` のときは何もしない（M7 までと完全に同一の経路）。
    // **fail-open**: lane を作れなくても enqueue は止めない（`FailPolicy::TENANT_LANE`）。
    // 止めると NATS の一時的な不調でテナントのジョブが一切受け付けられなくなる。
    // 取りこぼしは reconcile が最大 1 周期以内に収束させる。
    crate::lanes::ensure_lane_for_tenant(state, req.tenant).await;

    let mut headers = NatsHeaderMap::new();
    headers.insert("Nats-Msg-Id", req.execution_id.as_str());
    let ack = match state
        .jetstream()
        .publish_with_headers(invoke_subject(req.tenant), headers, payload.into())
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                tenant = %req.tenant,
                origin = %req.origin,
                error = %e,
                kind = ?e.kind(),
                "jetstream publish failed; signaling backpressure"
            );
            return Err(EnqueueError::PublishBackpressure);
        }
    };
    // PublishAck を待ち、stream が受理したことを確認する（落ちてもジョブは DB に記録済み）。
    if let Err(e) = ack.await {
        tracing::warn!(
            tenant = %req.tenant,
            origin = %req.origin,
            error = %e,
            kind = ?e.kind(),
            "jetstream publish ack failed; signaling backpressure"
        );
        return Err(EnqueueError::PublishBackpressure);
    }

    // M7a (§2.9): canary の計上は **publish ack 成功後のここ 1 箇所だけ**で行う。全入口
    // （HTTP / cron / event / chain）がこの関数を通るため、3 起点が自動的に、かつ「実際に起動した
    // 数」だけが計上される（解決時点で数えると 429 / presign 失敗 / 冪等ヒットまで混ざる）。
    state
        .metrics()
        .canary_routed_total
        .with_label_values(&[req.routing_reason])
        .inc();

    Ok(EnqueueOutcome::Enqueued {
        execution_id: req.execution_id,
    })
}

/// Postgres の unique_violation (23505) かどうか（冪等 race の backstop 判定）。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505"))
}

/// 実行状態文字列を `ExecutionStatus` へパースする（冪等ヒット応答用）。未知は `None`。
fn parse_execution_status(s: &str) -> Option<ExecutionStatus> {
    match s {
        "pending" => Some(ExecutionStatus::Pending),
        "running" => Some(ExecutionStatus::Running),
        "succeeded" => Some(ExecutionStatus::Succeeded),
        "failed" => Some(ExecutionStatus::Failed),
        "timeout" => Some(ExecutionStatus::Timeout),
        _ => None,
    }
}
