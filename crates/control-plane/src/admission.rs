//! invoke admission の per-class fail-mode 適用 (M3d, §8)。
//!
//! invoke は 2 つの admission ゲートを通る:
//! 1. token-bucket レート制限（per-tenant `invoke_rate`）。超過は 429 + `Retry-After`。
//! 2. in-flight 同時実行 reserve（atomic INCR-cmp-condDECR）。超過は 429。
//!
//! **fail-mode 分類 (§8 MUST)**: invoke のレート制限 / in-flight は [`FailPolicy::Open`]
//! （fail-open）。共有ストア（Redis）到達不能で invoke をブロックすると、性能制御のはずの
//! ゲートが可用性インシデントになる。よってストア到達不能（[`StoreError::is_unavailable`]）の
//! ときは **admit（素通し）** する。ただし縮退は **ログ + 監査（§3.7）に必ず記録** する
//! （無記録の fail-open は静かな上限喪失 = 重大インシデント）。
//!
//! ストアが応答したが想定外（[`StoreError::Backend`]）の場合も、可用性側に倒して admit する
//! が同様に縮退記録する（invoke を 500 にしない）。fail-closed なのは login だけ（login.rs）。
//!
//! `Retry-After` の計算はストアの token-bucket が返す `retry_after_secs`（次の 1 トークンが
//! 貯まるまでの秒数）をそのまま使う。HTTP は秒の整数を要求する（RFC 7231 §7.1.3）。

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::{AppState, ResolvedAdmissionParams};
use crate::store::{FailPolicy, StoreError};

/// 同時実行 429 の既定 Retry-After（秒）— M4d (§8)。
///
/// 仕様 §8 は「クォータ / レート超過時は `429`（`rate_limited`）で拒否し、`Retry-After` を付す」と
/// する MUST。token-bucket の枯渇では `Retry-After` を実際の補充時間から計算できるが、同時実行
/// 超過では「いつ空くか」は他実行に依存して未知である。保守的な小さな固定値（1 秒）を載せて
/// クライアントの自動リトライに最小限の手掛かりを与える。0 ならヘッダ省略になるためここは 1。
const CONCURRENCY_RETRY_AFTER_SECS: u64 = 1;

/// publish 失敗（JetStream backpressure / TimedOut / BrokenPipe）由来の 429 既定 Retry-After（秒）。
/// クライアントには「サーバ側で受付済みだが下流が詰まっている」状態を示す。tight loop での再送は
/// 状況を悪化させるため、レート制限よりやや長めに 5 秒を採用する（仕様 §8 line 749 のキュー
/// 滞留時のバックプレッシャ MUST に対応）。
const PUBLISH_BACKPRESSURE_RETRY_AFTER_SECS: u64 = 5;

/// レート制限超過の 429 応答。`Retry-After`（秒）ヘッダ + 既存のエラーエンベロープ
/// `{error:{code,message,retryable}}`（§6.5）を返す。`retryable=true`（時間をおけば回復する）。
///
/// `FaasError` には 429 バリアントが無いため（error.rs の写像は 4xx/5xx の固定集合）、
/// レート制限だけはハンドラ内でこの専用レスポンスを構築する（Retry-After を載せるためにも
/// 専用 Response が要る）。
#[derive(Debug, Clone, Copy)]
pub struct RateLimited {
    /// `Retry-After`（秒）。0 のときはヘッダを省く。
    pub retry_after_secs: u64,
    /// クライアント向けの安定コード（"rate_limited" or "concurrency_limit")。
    pub code: &'static str,
    /// 人間可読メッセージ。
    pub message: &'static str,
}

impl RateLimited {
    /// token-bucket 枯渇（per-tenant invoke_rate 超過）。
    pub fn rate(retry_after_secs: u64) -> Self {
        Self {
            retry_after_secs,
            code: "rate_limited",
            message: "invoke rate limit exceeded; retry later",
        }
    }

    /// in-flight 同時実行上限超過（max_concurrent_executions）。
    ///
    /// M4d (§8 MUST): 仕様は 429 拒否時に `Retry-After` を **必ず**付すと書かれているため、
    /// 同時実行 429 でもヘッダを省略しない。実際の解放時刻は他実行依存で未知だが、
    /// 保守的な短い既定値（[`CONCURRENCY_RETRY_AFTER_SECS`] = 1 秒）を載せて、自動リトライ
    /// クライアントに「すぐ再試行してよい」のヒントを与える。
    pub fn concurrency() -> Self {
        Self {
            retry_after_secs: CONCURRENCY_RETRY_AFTER_SECS,
            code: "concurrency_limit",
            message: "max concurrent executions reached; retry later",
        }
    }

    /// JetStream publish が backpressure / 過渡的接続障害で失敗した場合の 429 (M4d, §8)。
    ///
    /// publish 自体は MaxAckPending 制限を直接エラーとして返さない（メッセージはストリームに
    /// 入る）が、ストリーム書き込みの TimedOut / BrokenPipe / Other はサーバが詰まっている
    /// シグナルなので、500（リトライ抑制）ではなく 429（バックオフ後リトライ）で返す方が
    /// クライアント挙動として正しい（§8 line 749「キュー滞留時もバックプレッシャとして 429」）。
    pub fn publish_backpressure() -> Self {
        Self {
            retry_after_secs: PUBLISH_BACKPRESSURE_RETRY_AFTER_SECS,
            code: "publish_backpressure",
            message: "upstream queue is saturated; retry later",
        }
    }
}

impl IntoResponse for RateLimited {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "retryable": true,
            }
        }));
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
        if self.retry_after_secs > 0 {
            if let Ok(v) = header::HeaderValue::from_str(&self.retry_after_secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

/// admission ゲートの判定結果。`Admitted` は処理続行、`Rejected` は 429 を返す。
pub enum Decision {
    /// 許可（通常 admit、または fail-open による縮退 admit）。
    Admitted,
    /// 拒否（上限超過）。429 応答を保持する。
    Rejected(RateLimited),
}

/// fail-open ポリシー（invoke レート / in-flight）をストアエラーへ適用する (§8)。
///
/// ストアが落ちている（Unavailable）/ 想定外（Backend）のときは admit へ倒し、縮退を
/// ログ + 監査に記録する。**ここが §8 の「fail-open は許可するが必ず記録する」を満たす単一点**。
///
/// `class` は監査 detail に載せる admission クラス名（"invoke_rate" / "inflight"）。
async fn fail_open_degraded(state: &AppState, tenant: &str, class: &str, err: &StoreError) {
    // クラス別ポリシーが Open であることを型で再確認する（誤って Closed を渡さない）。
    debug_assert_eq!(
        FailPolicy::INVOKE_RATE,
        FailPolicy::Open,
        "invoke admission must be fail-open"
    );
    let unavailable = err.is_unavailable();
    tracing::warn!(
        tenant = %tenant,
        class = %class,
        unavailable,
        error = %err,
        "admission store degraded; failing OPEN (admitting) — limit temporarily not enforced (§8)"
    );
    // §3.7: 縮退を監査に残す（best-effort）。fail-open の静かな上限喪失を可観測にする。
    audit_degraded(state, tenant, class, unavailable).await;
}

/// 縮退を audit_logs に best-effort で追記する（§3.7）。専用短命 tx + GUC。
async fn audit_degraded(state: &AppState, tenant: &str, class: &str, unavailable: bool) {
    let detail = json!({
        "class": class,
        "fail_mode": "open",
        "reason": if unavailable { "store_unavailable" } else { "store_backend_error" },
    });
    let res: anyhow::Result<()> = async {
        let mut tx = state.pool().begin().await?;
        crate::db::set_tenant_guc(&mut tx, tenant).await?;
        crate::db::insert_audit_log(
            &mut *tx,
            tenant,
            None,
            "admission_degraded",
            None,
            Some(&detail),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, class, "failed to write admission_degraded audit row");
    }
}

/// token-bucket レート制限ゲート（fail-open, §8）。
///
/// 許可/縮退 admit なら `Admitted`、枯渇なら `Rejected(429 + Retry-After)`。
///
/// M4d: `params` は呼び出し側で per-tenant 上書きを反映した値（[`ResolvedAdmissionParams`]）を
/// 渡す。グローバル既定は `state.admission().resolve_for_tenant(tenant, &Default::default())` で
/// 得られる。テナント上書きを引いていない呼び出しは reaper / 内部ジョブのみが想定。
pub async fn check_rate_limit(
    state: &AppState,
    tenant: &str,
    params: &ResolvedAdmissionParams,
    now_ms: u64,
) -> Decision {
    match state.store().rate_limit(tenant, params.rate, now_ms).await {
        Ok(d) if d.allowed => Decision::Admitted,
        Ok(d) => Decision::Rejected(RateLimited::rate(d.retry_after_secs)),
        Err(e) => {
            // fail-open: 記録して admit。
            fail_open_degraded(state, tenant, "invoke_rate", &e).await;
            Decision::Admitted
        }
    }
}

/// in-flight 同時実行 reserve ゲート（fail-open, §8）。
///
/// 予約できたら（または縮退 admit）`Admitted`、上限超過なら `Rejected(429)`。縮退 admit の
/// ときは **予約していない**ため、呼び出し側は「予約できた本物の admit」と区別する必要がある:
/// 戻り値の `reserved` で示す（true=実際に +1 した → 失敗時に release が必要)。
///
/// M4d: `params` は per-tenant 上書きを反映した値（[`ResolvedAdmissionParams`]）を渡す。
pub async fn reserve_inflight(
    state: &AppState,
    tenant: &str,
    params: &ResolvedAdmissionParams,
) -> (Decision, bool) {
    match state
        .store()
        .reserve_inflight(tenant, params.inflight)
        .await
    {
        Ok(d) if d.admitted => (Decision::Admitted, true),
        Ok(_) => (Decision::Rejected(RateLimited::concurrency()), false),
        Err(e) => {
            // fail-open: 記録して admit。ただし実際の予約はしていない（reserved=false）。
            fail_open_degraded(state, tenant, "inflight", &e).await;
            (Decision::Admitted, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// 429 応答は Retry-After ヘッダ（秒）と retryable=true のエンベロープを持つ。
    #[tokio::test]
    async fn rate_limited_sets_retry_after_and_envelope() {
        let resp = RateLimited::rate(7).into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).unwrap(),
            &header::HeaderValue::from_static("7")
        );
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "rate_limited");
        assert_eq!(v["error"]["retryable"], true);
    }

    /// M4d (§8 MUST): concurrency 429 も Retry-After ヘッダを必ず持つ（既定 1 秒）。
    #[tokio::test]
    async fn concurrency_limit_includes_retry_after() {
        let resp = RateLimited::concurrency().into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = resp.headers().get(header::RETRY_AFTER).expect(
            "concurrency_limit must include Retry-After per §8 MUST: \
             '429 で拒否し、Retry-After を付す'",
        );
        assert_eq!(header, &header::HeaderValue::from_static("1"));
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "concurrency_limit");
        assert_eq!(v["error"]["retryable"], true);
    }

    /// M4d (§8): publish_backpressure も 429 + Retry-After を持つ（既定 5 秒）。
    #[tokio::test]
    async fn publish_backpressure_429_with_retry_after() {
        let resp = RateLimited::publish_backpressure().into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = resp.headers().get(header::RETRY_AFTER).expect(
            "publish_backpressure must include Retry-After per §8 MUST: \
             'キュー滞留時もバックプレッシャとして 429'",
        );
        assert_eq!(header, &header::HeaderValue::from_static("5"));
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "publish_backpressure");
        assert_eq!(v["error"]["retryable"], true);
    }

    /// Retry-After は token-bucket の retry_after_secs をそのまま秒で載せる。
    #[test]
    fn retry_after_is_passed_through_in_seconds() {
        assert_eq!(RateLimited::rate(0).retry_after_secs, 0);
        assert_eq!(RateLimited::rate(1).retry_after_secs, 1);
        assert_eq!(RateLimited::rate(3600).retry_after_secs, 3600);
    }

    /// M4d (§8 MUST): すべての 429 経路（rate_limited / concurrency_limit /
    /// publish_backpressure）が **同時に** Retry-After を載せ、status==429 を返す。
    /// 仕様の「429 で拒否し Retry-After を必ず付す」を 1 つの退行ガードに固める。
    #[tokio::test]
    async fn all_429_paths_include_retry_after_and_429_status() {
        for r in [
            RateLimited::rate(7),
            RateLimited::concurrency(),
            RateLimited::publish_backpressure(),
        ] {
            let resp = r.into_response();
            assert_eq!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "all admission rejection paths must return 429 (§8 MUST)"
            );
            assert!(
                resp.headers().contains_key(header::RETRY_AFTER),
                "all 429 responses must include Retry-After (§8 MUST)"
            );
            // Retry-After 値は秒の整数（RFC 7231 §7.1.3）。
            let v = resp.headers().get(header::RETRY_AFTER).unwrap();
            let v = v.to_str().unwrap();
            assert!(
                v.parse::<u64>().is_ok(),
                "Retry-After must be a delta-seconds integer per RFC 7231; got {v:?}"
            );
        }
    }

    /// M4d: rate_limited は token-bucket から渡された値をそのままヘッダに載せる。
    /// バックエンドの token-bucket 計算（store.rs）と Response 側の写像が合っていることを
    /// 担保する（ここがズレるとクライアントの自動リトライが暴発する）。
    #[tokio::test]
    async fn rate_retry_after_is_taken_from_token_bucket() {
        let resp = RateLimited::rate(42).into_response();
        let v = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("rate-limited 429 must include Retry-After");
        assert_eq!(v, &header::HeaderValue::from_static("42"));
    }
}
