//! 共有アプリケーション状態。
//!
//! axum ハンドラ / バックグラウンド task / 認証層が共有する。

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use faas_shared::ResultMessage;
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::db::TenantQuotaOverrides;
use crate::metrics::Metrics;
use crate::signing::{Signer, Verifier};
use crate::storage::Storage;
use crate::store::{InflightParams, LockoutParams, RateLimitParams, Store};

/// M4d (§8): 仕様 §8 表「上限（推奨）」。テナント上書き値が **上回ったら** ここで頭打ちにする
/// （クランプ + 警告ログ）。仕様 §8 で MUST レベルではない（運用 default + 推奨上限）が、
/// 過大な上書きで他テナント・JetStream MaxAckPending を巻き込まないよう **必ずクランプ**する。
pub const QUOTA_MAX_INVOKE_RATE_PER_SEC: u64 = 500;
/// 同上: 同時実行（pending+running）の運用上限。
pub const QUOTA_MAX_CONCURRENT_EXECUTIONS: u64 = 200;

/// admission 制御のパラメータ束（M3d, §8）。`Config` のグローバル既定から派生し、
/// invoke handler（レート制限 / in-flight）と login（ロックアウト）が参照する。
///
/// M4d (§8): per-tenant 上書きは [`AdmissionConfig::resolve_for_tenant`] で解決する。
/// グローバル既定 → テナント上書きの優先順位でフィールドごとにマージする（仕様 §8 表）。
#[derive(Debug, Clone, Copy)]
pub struct AdmissionConfig {
    /// invoke レート制限の token-bucket パラメータ（refill = invoke_rate, capacity = burst）。
    pub rate: RateLimitParams,
    /// in-flight 同時実行の上限 + カウンタ TTL。
    pub inflight: InflightParams,
    /// login 失敗ロックアウトの閾値 + 減衰窓（両キーに同値適用）。
    pub lockout: LockoutParams,
    /// X-Forwarded-For を信頼してクライアント IP を取り出すか（§6.0; 既定 false）。
    pub trust_proxy_headers: bool,
}

impl AdmissionConfig {
    /// M4d (§8): テナント上書きをグローバル既定にマージして per-tenant パラメータを返す。
    ///
    /// マージ規則:
    /// - `invoke_rate_per_sec` が Some → token-bucket の `refill_per_sec` を置き換え。
    /// - `invoke_burst` が Some → token-bucket の `capacity` を置き換え。
    /// - `max_concurrent_executions` が Some → in-flight の `max` を置き換え。
    /// - None / 欠損 / null → グローバル既定を継承（変更しない）。
    /// - login ロックアウトは per-tenant 上書きを持たない（全テナント共通の security 制御）。
    ///
    /// **上限クランプ** (§8 表「上限」): 上書きが推奨上限（[`QUOTA_MAX_INVOKE_RATE_PER_SEC`] /
    /// [`QUOTA_MAX_CONCURRENT_EXECUTIONS`]）を超えていたら頭打ちにして warn ログを残す。これは
    /// 過大設定で MaxAckPending や他テナントを巻き込まないための防御層（admin API 側でも検証する
    /// 想定だが、ここでも独立に強制することで両方が間違っても安全側に倒れる）。
    pub fn resolve_for_tenant(
        &self,
        tenant_id: &str,
        overrides: &TenantQuotaOverrides,
    ) -> ResolvedAdmissionParams {
        let mut rate = self.rate;
        let mut inflight = self.inflight;

        if let Some(rps) = overrides.invoke_rate_per_sec {
            let clamped = clamp_quota_u64(
                tenant_id,
                "invoke_rate_per_sec",
                rps,
                QUOTA_MAX_INVOKE_RATE_PER_SEC,
            );
            rate.refill_per_sec = clamped as f64;
        }
        if let Some(burst) = overrides.invoke_burst {
            // バーストは「秒間平均 * バースト窓」を許容する量で、特に上限定数は無いが、
            // 過大設定はメモリ・stampede の温床になるので推奨上限 * 10 まで（観測のみ）。
            rate.capacity = burst as f64;
        }
        if let Some(max) = overrides.max_concurrent_executions {
            let clamped = clamp_quota_u64(
                tenant_id,
                "max_concurrent_executions",
                max,
                QUOTA_MAX_CONCURRENT_EXECUTIONS,
            );
            inflight.max = clamped as i64;
        }

        ResolvedAdmissionParams { rate, inflight }
    }
}

/// 与えられた `value` が `max` を超えていたらクランプし warn ログを残す（M4d）。
/// 純関数（テスト用）。
fn clamp_quota_u64(tenant: &str, field: &str, value: u64, max: u64) -> u64 {
    if value > max {
        tracing::warn!(
            tenant = %tenant,
            field,
            value,
            max,
            "tenant quota override exceeds recommended upper bound; clamping to max"
        );
        max
    } else {
        value
    }
}

/// M4d (§8): per-tenant に解決した admission パラメータ。invoke handler で 1 度だけ
/// 計算し、レート制限 / in-flight reserve の両ゲートに渡す。
#[derive(Debug, Clone, Copy)]
pub struct ResolvedAdmissionParams {
    pub rate: RateLimitParams,
    pub inflight: InflightParams,
}

/// M6a (§15): 同期 invoke の per-instance waiter registry の型エイリアス。
///
/// correlation_id（同期 invoke 1 回ごとに採番した不透明 ID）→ 結果を 1 回だけ受け取る
/// `oneshot::Sender<ResultMessage>` の並行マップ。invoke ハンドラが publish 後に insert し、
/// reply 購読タスクが reply 到達時に remove して send する。**per-instance（共有しない）** で、
/// reply subject に埋め込んだ `instance_id` により「JobMessage を送った当該インスタンスだけが
/// reply を受け取る」を成立させる（ステートレス×N の鍵, §4 不変条件）。`DashMap` でロック競合を
/// 避けつつ、timeout 時はハンドラ自身が除去して leak を防ぐ。
pub type WaiterRegistry = Arc<DashMap<String, oneshot::Sender<ResultMessage>>>;

/// ハンドラへ注入する共有状態。`Clone` は内部 `Arc` により安価。
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pool: PgPool,
    nats: async_nats::Client,
    /// invoke を Nats-Msg-Id=execution_id 付きで publish するための JetStream context
    /// （冪等性 layer 3, §6.6）。
    jetstream: async_nats::jetstream::Context,
    storage: Storage,
    max_wasm_upload_bytes: u64,
    presign_ttl: Duration,
    /// /uploads の presigned PUT URL の TTL（§3.4 / §5.2）。worker への wasm presign GET とは
    /// 別管理（用途・寿命が異なる）。
    upload_presign_ttl: Duration,
    /// bootstrap system-admin トークン（POST /admin/tenants を gate）。
    bootstrap_admin_token: String,
    /// login の no-user/no-tenant パスで使う固定ダミー argon2 ハッシュ
    /// （timing oracle 防止のため常に verify する）。
    dummy_password_hash: String,
    /// 共有 admission ストア（M3d, §8）。invoke レート制限 / in-flight / login ロックアウトを
    /// 全 Axum インスタンスで共有して集計する。invoke handler / login / reaper が参照する。
    store: Arc<dyn Store>,
    /// admission 制御パラメータ（rate / inflight / lockout, §8）。
    admission: AdmissionConfig,
    /// ジョブ署名トークンの署名器（M3c, §3.3）。invoke が mint、subscriber が verify する。
    signer: Arc<Signer>,
    /// invoke が壁時計上限から token exp を計算するためのオフセット計算器。
    /// `Config::token_exp_offset_secs` をクロージャ化して持つ（Config 全体を抱えない）。
    token_exp_offset_secs: Box<dyn Fn(u64) -> i64 + Send + Sync>,
    /// 観測メトリクス（M4a, §3.8）。`/metrics` ハンドラ + 計装点（invoke / finalize / reaper）から
    /// 参照する。Registry はプロセスで 1 つ。Arc で共有して clone コストをゼロに抑える。
    metrics: Arc<Metrics>,
    /// M6a (§15): この CP インスタンスの subject-safe 識別子（env `INSTANCE_ID` or `inst_{uuid}`）。
    /// 同期 invoke の reply subject `reply.{instance_id}.{correlation_id}` に埋め込む。
    instance_id: String,
    /// M6a (§15): 同期 invoke の待機上限。超過でクライアントへ 202 + execution_id へフォールバックする。
    sync_reply_timeout: Duration,
    /// M6a (§15): 同期 invoke の per-instance waiter registry（correlation_id -> oneshot::Sender）。
    waiters: WaiterRegistry,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        nats: async_nats::Client,
        storage: Storage,
        max_wasm_upload_bytes: u64,
        presign_ttl_secs: u64,
        upload_presign_ttl_secs: u64,
        bootstrap_admin_token: String,
        dummy_password_hash: String,
        signer: Arc<Signer>,
        token_exp_offset_secs: Box<dyn Fn(u64) -> i64 + Send + Sync>,
        store: Arc<dyn Store>,
        admission: AdmissionConfig,
        metrics: Arc<Metrics>,
        instance_id: String,
        sync_reply_timeout_ms: u64,
    ) -> Self {
        // invoke の JetStream publish 用 context は NATS クライアントから構築する。
        let jetstream = async_nats::jetstream::new(nats.clone());
        Self {
            inner: Arc::new(Inner {
                pool,
                nats,
                jetstream,
                storage,
                max_wasm_upload_bytes,
                presign_ttl: Duration::from_secs(presign_ttl_secs),
                upload_presign_ttl: Duration::from_secs(upload_presign_ttl_secs),
                bootstrap_admin_token,
                dummy_password_hash,
                signer,
                token_exp_offset_secs,
                store,
                admission,
                metrics,
                instance_id,
                sync_reply_timeout: Duration::from_millis(sync_reply_timeout_ms),
                // 同期 invoke の waiter registry はプロセス起動時に空で作る（per-instance, M6a）。
                waiters: Arc::new(DashMap::new()),
            }),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.inner.pool
    }

    pub fn nats(&self) -> &async_nats::Client {
        &self.inner.nats
    }

    /// invoke を publish する JetStream context（Nats-Msg-Id 冪等, §6.6）。
    pub fn jetstream(&self) -> &async_nats::jetstream::Context {
        &self.inner.jetstream
    }

    /// ジョブ署名器（invoke が sign、subscriber が verify に使う）。
    pub fn signer(&self) -> &Signer {
        &self.inner.signer
    }

    /// 署名トークンの検証器（subscriber が使う）。
    pub fn verifier(&self) -> &Verifier {
        self.inner.signer.verifier()
    }

    /// 壁時計上限（ms）から token exp オフセット秒数を計算する (§3.3)。
    pub fn token_exp_offset_secs(&self, wall_time_ms: u64) -> i64 {
        (self.inner.token_exp_offset_secs)(wall_time_ms)
    }

    /// Object Storage クライアント（本体保存 / presign, §3.4）。
    pub fn storage(&self) -> &Storage {
        &self.inner.storage
    }

    /// wasm 本体の最大アップロードサイズ（bytes, §6.2）。
    pub fn max_wasm_upload_bytes(&self) -> u64 {
        self.inner.max_wasm_upload_bytes
    }

    /// presigned GET URL の TTL（§3.4）。
    pub fn presign_ttl(&self) -> Duration {
        self.inner.presign_ttl
    }

    /// /uploads の presigned PUT URL の TTL（§3.4 / §5.2 / §6.4）。
    pub fn upload_presign_ttl(&self) -> Duration {
        self.inner.upload_presign_ttl
    }

    /// bootstrap system-admin トークン（POST /admin/tenants の gate, §3.3）。
    pub fn bootstrap_admin_token(&self) -> &str {
        &self.inner.bootstrap_admin_token
    }

    /// login の no-user パスで使う固定ダミー argon2 ハッシュ（timing 均一化）。
    pub fn dummy_password_hash(&self) -> &str {
        &self.inner.dummy_password_hash
    }

    /// 共有 admission ストア（M3d, §8）。invoke レート制限 / in-flight / login ロックアウト。
    pub fn store(&self) -> &dyn Store {
        self.inner.store.as_ref()
    }

    /// admission 制御パラメータ（rate / inflight / lockout, §8）。
    pub fn admission(&self) -> &AdmissionConfig {
        &self.inner.admission
    }

    /// 観測メトリクス（M4a, §3.8）。`/metrics` ハンドラ + 計装点（invoke / finalize / reaper）。
    pub fn metrics(&self) -> &Metrics {
        self.inner.metrics.as_ref()
    }

    /// M6a (§15): この CP インスタンスの subject-safe 識別子。同期 invoke の reply subject
    /// `reply.{instance_id}.{correlation_id}` 構築と reply 購読 wildcard に使う。
    pub fn instance_id(&self) -> &str {
        &self.inner.instance_id
    }

    /// M6a (§15): 同期 invoke の待機上限。invoke ハンドラが `tokio::time::timeout` に渡す。
    pub fn sync_reply_timeout(&self) -> Duration {
        self.inner.sync_reply_timeout
    }

    /// M6a (§15): 同期 invoke の per-instance waiter registry（correlation_id -> oneshot::Sender）。
    ///
    /// invoke ハンドラ（publish 後に insert / timeout 時に remove）と reply 購読タスク（reply 到達時に
    /// remove して send）の両方が触る。clone は内部 `Arc` なので安価。
    pub fn waiters(&self) -> &WaiterRegistry {
        &self.inner.waiters
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AdmissionConfig {
        AdmissionConfig {
            rate: RateLimitParams {
                refill_per_sec: 50.0,
                capacity: 500.0,
            },
            inflight: InflightParams {
                max: 20,
                ttl_secs: 3600,
            },
            lockout: LockoutParams {
                threshold: 10,
                window_secs: 900,
            },
            trust_proxy_headers: false,
        }
    }

    /// 上書き None はグローバル既定を変えない（継承）。
    #[test]
    fn resolve_inherits_when_no_overrides() {
        let c = cfg();
        let overrides = TenantQuotaOverrides::default();
        let r = c.resolve_for_tenant("ten_a", &overrides);
        assert_eq!(r.rate.refill_per_sec, 50.0);
        assert_eq!(r.rate.capacity, 500.0);
        assert_eq!(r.inflight.max, 20);
    }

    /// 各フィールドが独立に上書きされる（部分上書き）。
    #[test]
    fn resolve_applies_partial_overrides() {
        let c = cfg();
        let overrides = TenantQuotaOverrides {
            invoke_rate_per_sec: Some(100),
            invoke_burst: None,
            max_concurrent_executions: Some(50),
        };
        let r = c.resolve_for_tenant("ten_a", &overrides);
        assert_eq!(r.rate.refill_per_sec, 100.0);
        // burst は継承（既定）。
        assert_eq!(r.rate.capacity, 500.0);
        assert_eq!(r.inflight.max, 50);
        // TTL も継承（上書きキーが無いため）。
        assert_eq!(r.inflight.ttl_secs, 3600);
    }

    /// §8 表の上限を超えたらクランプする（防御）。
    #[test]
    fn resolve_clamps_over_recommended_upper_bound() {
        let c = cfg();
        let overrides = TenantQuotaOverrides {
            invoke_rate_per_sec: Some(10_000),
            invoke_burst: None,
            max_concurrent_executions: Some(10_000),
        };
        let r = c.resolve_for_tenant("ten_a", &overrides);
        assert_eq!(r.rate.refill_per_sec, QUOTA_MAX_INVOKE_RATE_PER_SEC as f64);
        assert_eq!(r.inflight.max, QUOTA_MAX_CONCURRENT_EXECUTIONS as i64);
    }

    /// 上限ちょうど（境界）はクランプされない。
    #[test]
    fn resolve_at_boundary_is_not_clamped() {
        let c = cfg();
        let overrides = TenantQuotaOverrides {
            invoke_rate_per_sec: Some(QUOTA_MAX_INVOKE_RATE_PER_SEC),
            invoke_burst: None,
            max_concurrent_executions: Some(QUOTA_MAX_CONCURRENT_EXECUTIONS),
        };
        let r = c.resolve_for_tenant("ten_a", &overrides);
        assert_eq!(r.rate.refill_per_sec, QUOTA_MAX_INVOKE_RATE_PER_SEC as f64);
        assert_eq!(r.inflight.max, QUOTA_MAX_CONCURRENT_EXECUTIONS as i64);
    }

    /// M4d (§8): per-tenant 上書き値はグローバル既定を「下回る」ケースでも正しく勝ち、
    /// 実際に admission ゲート（InProcStore.rate_limit）の挙動が変わる。
    /// 仕様文言「グローバル既定 → テナント上書きの優先順位」の振る舞い側 e2e 検証。
    #[tokio::test]
    async fn tenant_override_beats_global_default_in_admission() {
        use crate::store::{InProcStore, Store};

        // グローバル既定は 50rps / burst 500（cfg() の値）。
        let c = cfg();
        // テナントは厳しい上限（1rps / burst 1）に絞り込みたい。
        let overrides = TenantQuotaOverrides {
            invoke_rate_per_sec: Some(1),
            invoke_burst: Some(1),
            max_concurrent_executions: None,
        };
        let resolved = c.resolve_for_tenant("ten_a", &overrides);
        // 上書き値がグローバル既定を下回っていることを担保（テストの前提）。
        assert!(resolved.rate.refill_per_sec < c.rate.refill_per_sec);
        assert!(resolved.rate.capacity < c.rate.capacity);

        let store = InProcStore::new();
        let now_ms = 1_000_000u64;
        // 1 度目は burst 内で許可。
        let d1 = store
            .rate_limit("ten_a", resolved.rate, now_ms)
            .await
            .unwrap();
        assert!(d1.allowed);
        // 2 度目は burst (1) 越え → 上書き値が効いていれば拒否（Retry-After > 0）。
        // グローバル既定（50rps / burst 500）が効いていたら 2 度目も通ってしまうため、
        // ここが本テストの核心アサート。
        let d2 = store
            .rate_limit("ten_a", resolved.rate, now_ms)
            .await
            .unwrap();
        assert!(
            !d2.allowed,
            "tenant override (burst=1) must clamp below the global default (burst=500)"
        );
        assert!(d2.retry_after_secs >= 1, "Retry-After must be >= 1 second");
    }

    /// M4d: 異なるテナントは異なる解決済みパラメータを持ち、互いに影響しない（権限境界の維持）。
    #[test]
    fn resolve_is_per_tenant_independent() {
        let c = cfg();
        let strict = TenantQuotaOverrides {
            invoke_rate_per_sec: Some(1),
            invoke_burst: Some(1),
            max_concurrent_executions: Some(1),
        };
        let loose = TenantQuotaOverrides::default();
        let r_strict = c.resolve_for_tenant("ten_a", &strict);
        let r_loose = c.resolve_for_tenant("ten_b", &loose);
        assert_ne!(r_strict.rate.refill_per_sec, r_loose.rate.refill_per_sec);
        assert_ne!(r_strict.inflight.max, r_loose.inflight.max);
        // 別テナントへ上書き値が「漏れない」ことの最低限の保証。
        assert_eq!(r_loose.rate.refill_per_sec, c.rate.refill_per_sec);
        assert_eq!(r_loose.inflight.max, c.inflight.max);
    }
}
