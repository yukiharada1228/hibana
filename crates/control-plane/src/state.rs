//! 共有アプリケーション状態。
//!
//! axum ハンドラ / バックグラウンド task / 認証層が共有する。

use std::sync::atomic::{AtomicU64, Ordering};
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
/// M8 (§4.2): per-lane 実行クレジットの運用上限。
///
/// 実効値は worker 側の `effective_lane_concurrency` が「プロセス全体上限 ÷ 購読 lane 数」で
/// さらに頭打ちにするため、ここは「テナントが希望できる上限」でしかない。過大な希望値が
/// consumer metadata を通じて worker の計算へ流れ込まないようクランプする。
pub const QUOTA_MAX_LANE_CONCURRENCY: u64 = 32;

/// lane reconcile の advisory lock キー（プロセス間で固定・衝突しない値）。
///
/// 他の advisory lock 用途が増えたときのために、値の由来をここに明記しておく:
/// "faas" + M8 のマイルストン番号を並べただけの固定値であり、意味は無い（一意であればよい）。
const LANE_RECONCILE_LOCK_KEY: i64 = 0x0FAA_5008;

/// lane reconcile の単一 writer ロック。drop で解放する。
pub struct LaneReconcileGuard {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

impl Drop for LaneReconcileGuard {
    fn drop(&mut self) {
        // セッションロックなので、接続を返す前に明示的に解放する。接続がプールへ戻って
        // 別の用途で使われたときにロックを持ち越さないため。
        if let Some(mut conn) = self.conn.take() {
            tokio::spawn(async move {
                let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                    .bind(LANE_RECONCILE_LOCK_KEY)
                    .execute(&mut *conn)
                    .await;
            });
        }
    }
}

/// M8 (§3.7): lane provisioning の設定束。`Config` から派生する。
#[derive(Debug, Clone)]
pub struct LaneConfig {
    /// テナント別 lane を有効にするか（false = M7 までと同一トポロジ）。
    pub enabled: bool,
    /// 専有 lane の上限数。超過分は overflow lane 1 本へ束ねる。
    pub max_dedicated: u64,
    /// lane consumer の `max_ack_pending` に足す余裕。
    pub ack_pending_headroom: u64,
    /// overflow lane の `max_ack_pending`（固定値）。
    pub overflow_ack_pending: u64,
    /// consumer の ack_wait 秒（トークン exp と同一定数から導出, §3.3）。
    pub ack_wait_secs: u64,
    /// consumer の最大再配送回数。
    pub max_deliver: u64,
    /// 再配送 backoff（秒）。
    pub backoff_secs: Vec<u64>,
    /// lane gauge に `lane` ラベルを付けるか（§6.2）。false なら `"aggregate"` 1 値に畳む。
    pub metrics_lane_labels: bool,
}

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
    /// M8 (§4.2): per-lane 実行クレジットのグローバル既定（`WORKER_LANE_CONCURRENCY`）。
    /// テナント上書きが無ければこの値が consumer metadata 経由で worker へ配られる。
    pub lane_concurrency: u64,
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

        let mut lane_concurrency = self.lane_concurrency;
        if let Some(lc) = overrides.lane_concurrency {
            lane_concurrency = clamp_quota_u64(
                tenant_id,
                "lane_concurrency",
                lc,
                QUOTA_MAX_LANE_CONCURRENCY,
            )
            // 0 は「lane が永久に停止する」ことを意味するので最低 1 に倒す。
            .max(1);
        }

        ResolvedAdmissionParams {
            rate,
            inflight,
            lane_concurrency,
        }
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
    /// M8 (§4.2): worker 1 プロセスがこのテナントの lane に同時に割ける実行スロットの希望値。
    /// consumer metadata 経由で worker へ配る（M8-4 が消費する）。
    #[allow(dead_code)]
    pub lane_concurrency: u64,
}

impl ResolvedAdmissionParams {
    /// M8 (§3.5): このテナントの lane consumer に設定する `max_ack_pending`。
    ///
    /// **テナント自身の in-flight 上限 + headroom** で導出する。M7 までは全テナント合算の
    /// 固定値 1000 だったため、`Σ(テナント数 × max_concurrent_executions)` がそれを超えると
    /// **1 件しか投げていないテナントが他テナントの負荷で配送されない**（そして pending が
    /// 減らないので、やがて自分が 429 になる）という、完了条件に真正面から違反する経路があった。
    /// lane ごとに導出すれば、この合算の頭打ちが構造的に消える。
    ///
    /// headroom は「in-flight 上限ちょうどだと、終端と次の配送が重なる瞬間に配送が止まる」のを
    /// 避けるための余裕である。
    #[allow(dead_code)] // M8-4 の lane provisioning が唯一の呼び出し元になる。
    pub fn lane_ack_pending(&self, headroom: u64) -> i64 {
        (self.inflight.max.max(0) as u64).saturating_add(headroom) as i64
    }
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
    /// M7c (§4.6): `/internal/job-env` の per-IP / per-tenant レート上限（req/分）。
    job_env_exchange_rate_per_min: u64,
    /// M8 (§3.7): lane provisioning の設定束（reconcile と enqueue ensure が参照する）。
    lanes: LaneConfig,
    /// M8 (§3.7.3): 「この lane は既に在る」ことのプロセスローカルなキャッシュ。
    ///
    /// **世代カウンタとセットで使う**。reconcile が lane を 1 本でも作成 / 削除したら世代を
    /// バンプし、次の ensure がキャッシュを丸ごと捨てる。これが無いと
    /// 「suspend → reconcile が lane 削除 → 再 activate」の後にキャッシュヒットで二度と
    /// lane を作り直さず、そのテナントのジョブが**無音で配送されなくなる**。
    lane_cache: DashMap<String, u64>,
    /// lane トポロジの世代。reconcile が変更を加えるたびに +1 する。
    lane_generation: AtomicU64,
    /// reconcile が最後に観測した dedicated lane 数（ホットパスで assign_lanes を評価しないため）。
    dedicated_lane_count: AtomicU64,
    /// M7c (§10 / §15): secret の KEK キーリング。**control-plane だけが持つ**（worker は
    /// keyless by design, §3.3）。暗号化は常に active kid、復号は行の kid で選ぶ。
    secret_keyring: Arc<crate::secrets::SecretKeyring>,
    /// M10 follow-up (§3.8 / §6.2): `faas_tenant_invoke_total` に `tenant_id` ラベルを付けるか。
    /// 既定 true（従来挙動）。false でテナント数に比例する系列爆発を防ぐ（`"aggregate"` に畳む）。
    /// M8 の `METRICS_LANE_LABELS` と同じ思想（テナント数と一緒に伸びる軸に逃げ道を用意する）。
    metrics_include_tenant_label: bool,
    /// M11 (§4.2): 公開 HTTP ingress gateway のベースドメイン。None で gateway 無効。
    ingress_base_domain: Option<String>,
    /// M8 (§5): オートスケールの方針（env 由来・不変）。
    scale_policy: crate::scale::ScalePolicy,
    /// M8 (§5): backlog ポーラが書き、`GET /internal/scale` が読む共有スナップショット。
    ///
    /// `std::sync::Mutex` で足りる。中身は数値 4 つで、ロックを持ったまま await しない
    /// （async な Mutex を使うと「観測ループが HTTP ハンドラを待つ」構造ができてしまう）。
    scale_snapshot: std::sync::Mutex<ScaleSnapshot>,
}

/// M8 (§5.2 / §5.4): 観測と露出のあいだで受け渡す最小の状態。
#[derive(Debug, Clone, Copy)]
pub struct ScaleSnapshot {
    pub backlog: u64,
    pub lanes: usize,
    /// 最後に成功した観測の時刻。`None` = 一度も観測できていない。
    pub last_ok: Option<std::time::Instant>,
    /// 判断ロジックの持ち越し状態。
    pub state: crate::scale::ScaleState,
    /// 直近の判断結果。まだ判断していなければ `None`。
    pub last_decision: Option<crate::scale::Decision>,
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
        secret_keyring: Arc<crate::secrets::SecretKeyring>,
        job_env_exchange_rate_per_min: u64,
        lanes: LaneConfig,
        scale_policy: crate::scale::ScalePolicy,
        metrics_include_tenant_label: bool,
        ingress_base_domain: Option<String>,
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
                secret_keyring,
                job_env_exchange_rate_per_min,
                metrics_include_tenant_label,
                ingress_base_domain,
                lanes,
                lane_cache: DashMap::new(),
                lane_generation: AtomicU64::new(0),
                dedicated_lane_count: AtomicU64::new(0),
                scale_policy,
                scale_snapshot: std::sync::Mutex::new(ScaleSnapshot {
                    backlog: 0,
                    lanes: 0,
                    last_ok: None,
                    // 初期 target は 0 ではなく min_workers（§5.3 `ScaleState::new` の doc 参照）。
                    state: crate::scale::ScaleState::new(&scale_policy),
                    last_decision: None,
                }),
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

    // --- M8 (§3.7): lane provisioning ---

    /// テナント別 lane が有効か。
    pub fn tenant_lanes_enabled(&self) -> bool {
        self.inner.lanes.enabled
    }

    /// lane gauge に `lane` ラベルを付けるか（§6.2）。
    pub fn metrics_lane_labels(&self) -> bool {
        self.inner.lanes.metrics_lane_labels
    }

    /// `faas_tenant_invoke_total` に `tenant_id` ラベルを付けるか（M10 follow-up）。
    /// false のときは `"aggregate"` に畳んで系列爆発を防ぐ。
    pub fn metrics_include_tenant_label(&self) -> bool {
        self.inner.metrics_include_tenant_label
    }

    /// M11 (§4.2): 公開 ingress gateway のベースドメイン。None なら gateway 無効。
    pub fn ingress_base_domain(&self) -> Option<&str> {
        self.inner.ingress_base_domain.as_deref()
    }

    /// 専有 lane の上限数。
    pub fn max_dedicated_lanes(&self) -> usize {
        self.inner.lanes.max_dedicated as usize
    }

    /// lane consumer の `max_ack_pending` に足す余裕。
    pub fn lane_ack_pending_headroom(&self) -> u64 {
        self.inner.lanes.ack_pending_headroom
    }

    /// overflow lane / legacy consumer の `max_ack_pending`。
    pub fn lane_overflow_ack_pending(&self) -> i64 {
        self.inner.lanes.overflow_ack_pending as i64
    }

    /// lane consumer の配送パラメータ（ack_wait 秒 / max_deliver / backoff 秒）。
    ///
    /// M8 で consumer の作成者が control-plane へ移ったため、**トークン exp と再配送間隔の
    /// TTL 結合（§3.3）の責任も control-plane が持つ**。worker と値をずらしてはならない。
    pub fn lane_delivery_params(&self) -> (u64, u64, Vec<u64>) {
        (
            self.inner.lanes.ack_wait_secs,
            self.inner.lanes.max_deliver,
            self.inner.lanes.backoff_secs.clone(),
        )
    }

    /// lane ensure キャッシュに載っているか（**現世代のエントリだけ有効**）。
    pub fn lane_cache_contains(&self, tenant: &str) -> bool {
        let gen = self.inner.lane_generation.load(Ordering::Relaxed);
        self.inner
            .lane_cache
            .get(tenant)
            .map(|e| *e.value() == gen)
            .unwrap_or(false)
    }

    /// lane ensure キャッシュへ現世代で載せる。
    pub fn lane_cache_insert(&self, tenant: &str) {
        let gen = self.inner.lane_generation.load(Ordering::Relaxed);
        self.inner.lane_cache.insert(tenant.to_string(), gen);
    }

    /// lane トポロジの世代をバンプする（= 既存キャッシュを一斉に無効化する）。
    pub fn bump_lane_generation(&self) {
        self.inner.lane_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// reconcile が観測した dedicated lane 数を記録する。
    pub fn set_dedicated_lane_count(&self, n: usize) {
        self.inner
            .dedicated_lane_count
            .store(n as u64, Ordering::Relaxed);
    }

    /// 専有 lane 枠に空きがあるか（enqueue ホットパスの判定。`assign_lanes` を評価しない）。
    pub fn has_dedicated_lane_capacity(&self) -> bool {
        (self.inner.dedicated_lane_count.load(Ordering::Relaxed) as usize)
            < self.max_dedicated_lanes()
    }

    /// M8 (§5): スケール方針（env 由来・不変）。
    pub fn scale_policy(&self) -> &crate::scale::ScalePolicy {
        &self.inner.scale_policy
    }

    /// M8 (§5.2): ポーラが 1 周期ぶんの観測を反映し、判断ロジックを 1 歩進める。
    ///
    /// 判断結果を返すのは、呼び出し側が gauge に出すためである（gauge への書き込みまで
    /// ここでやると、状態と観測の責務が混ざる）。
    pub fn observe_backlog(&self, backlog: u64, lanes: usize) -> crate::scale::Decision {
        let now = std::time::Instant::now();
        let mut snap = self.lock_scale_snapshot();
        snap.backlog = backlog;
        snap.lanes = lanes;
        snap.last_ok = Some(now);
        let sig = crate::scale::ScaleSignal {
            backlog,
            age_secs: 0,
            ever_observed: true,
        };
        // 時計は「プロセス起動からの単調秒」を使う。壁時計を使うと NTP の巻き戻しで
        // cooldown の計時が壊れる。
        let decision = crate::scale::decide(
            &self.inner.scale_policy,
            &sig,
            &mut snap.state,
            monotonic_secs(),
        );
        snap.last_decision = Some(decision);
        decision
    }

    /// M8 (§5.4): `GET /internal/scale` が読む現在値。
    ///
    /// **観測が失敗している間も backlog を 0 に落とさない**（前回値を保持する）。
    /// 0 に落とすと「仕事が無い」と誤読され、NATS の一時的な瞬断がそのまま
    /// scale-to-zero を誘発する。代わりに `age_secs` が伸び、判断ロジックが hold に落ちる。
    pub fn scale_view(&self) -> (ScaleSnapshot, crate::scale::Decision) {
        let mut snap = self.lock_scale_snapshot();
        let age_secs = snap
            .last_ok
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(u64::MAX);
        let sig = crate::scale::ScaleSignal {
            backlog: snap.backlog,
            age_secs,
            ever_observed: snap.last_ok.is_some(),
        };
        let decision = crate::scale::decide(
            &self.inner.scale_policy,
            &sig,
            &mut snap.state,
            monotonic_secs(),
        );
        snap.last_decision = Some(decision);
        (*snap, decision)
    }

    /// M8: `scale_signal_age_seconds` gauge 用。観測の鮮度だけを取り出す。
    pub fn scale_signal_age_secs(&self) -> u64 {
        self.lock_scale_snapshot()
            .last_ok
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(u64::MAX)
    }

    /// poisoned な Mutex から復帰する。
    ///
    /// 中身は数値だけで不変条件を持たないので、パニックした書き手が壊した「途中の状態」は
    /// 存在しない。ここで panic を伝播させると、観測ループの一度の事故が
    /// **`/internal/scale` を恒久的に落とす**ことになるので、明示的に握って続行する。
    fn lock_scale_snapshot(&self) -> std::sync::MutexGuard<'_, ScaleSnapshot> {
        self.inner
            .scale_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// lane reconcile の単一 writer ロックを試みる (§3.7.1)。
    ///
    /// `pg_try_advisory_lock` のセッションロックを**専用接続**の上で取る。パス中に NATS の
    /// RPC を挟むため tx は張らない。取れなければ `None`（他インスタンスに任せる）。
    /// 返り値の [`LaneReconcileGuard`] は drop 時に `pg_advisory_unlock` を試みる。
    pub async fn try_lane_reconcile_lock(&self) -> anyhow::Result<Option<LaneReconcileGuard>> {
        use sqlx::Row as _;
        let mut conn = self.pool().acquire().await?;
        let got: bool = sqlx::query("SELECT pg_try_advisory_lock($1) AS locked")
            .bind(LANE_RECONCILE_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await?
            .try_get("locked")?;
        if got {
            Ok(Some(LaneReconcileGuard { conn: Some(conn) }))
        } else {
            Ok(None)
        }
    }

    /// `/internal/job-env` のレート上限（req/分）。
    pub fn job_env_exchange_rate_per_min(&self) -> u64 {
        self.inner.job_env_exchange_rate_per_min
    }

    /// secret の KEK キーリング (M7c)。**平文の鍵素材はここから外へ出さない**
    /// （`secrets::encrypt` / `decrypt` / `rewrap` が参照するだけ）。
    pub fn secret_keyring(&self) -> &crate::secrets::SecretKeyring {
        &self.inner.secret_keyring
    }

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

/// プロセス起動からの単調経過秒。
///
/// スケール判断のヒステリシスは**壁時計を使ってはならない**。NTP の巻き戻しで
/// `now - below_since` が負方向に飛ぶと、cooldown が満たされないまま永久に待つか、
/// 逆に一瞬で満たされて flapping する。単調時計なら両方起きない。
fn monotonic_secs() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
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
            lane_concurrency: 4,
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
            lane_concurrency: None,
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
            lane_concurrency: None,
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
            lane_concurrency: None,
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
            lane_concurrency: None,
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
            lane_concurrency: None,
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

    // ---- M8-2: lane_concurrency のマージ / クランプ / 導出（§4.2 / §3.5）----------

    #[test]
    fn lane_concurrency_falls_back_to_global_default() {
        let cfg = cfg();
        let r = cfg.resolve_for_tenant("ten_a", &TenantQuotaOverrides::default());
        assert_eq!(
            r.lane_concurrency, 4,
            "override 無しならグローバル既定を継承する"
        );
    }

    #[test]
    fn lane_concurrency_override_is_applied_and_clamped() {
        let cfg = cfg();

        let r = cfg.resolve_for_tenant(
            "ten_a",
            &TenantQuotaOverrides {
                lane_concurrency: Some(8),
                ..Default::default()
            },
        );
        assert_eq!(r.lane_concurrency, 8);

        // 推奨上限でクランプされる（過大な希望値が worker の計算へ流れ込まない）。
        let r = cfg.resolve_for_tenant(
            "ten_a",
            &TenantQuotaOverrides {
                lane_concurrency: Some(9_999),
                ..Default::default()
            },
        );
        assert_eq!(r.lane_concurrency, QUOTA_MAX_LANE_CONCURRENCY);

        // 0 は lane を永久停止させるので最低 1 に倒す。
        let r = cfg.resolve_for_tenant(
            "ten_a",
            &TenantQuotaOverrides {
                lane_concurrency: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(r.lane_concurrency, 1);
    }

    /// lane の `max_ack_pending` は **テナント自身の in-flight 上限 + headroom** で導出する。
    ///
    /// M7 までは全テナント合算の固定値 1000 で、`Σ(テナント数 × max_concurrent)` がそれを超えると
    /// 1 件しか投げていないテナントが他テナントの負荷で配送されなくなった（完了条件違反）。
    #[test]
    fn lane_ack_pending_is_inflight_plus_headroom() {
        let cfg = cfg();
        let r = cfg.resolve_for_tenant("ten_a", &TenantQuotaOverrides::default());
        assert_eq!(r.lane_ack_pending(8), r.inflight.max + 8);

        // 上書きした in-flight にも追随する（合算ではなくテナント単位である証拠）。
        let r = cfg.resolve_for_tenant(
            "ten_a",
            &TenantQuotaOverrides {
                max_concurrent_executions: Some(50),
                ..Default::default()
            },
        );
        assert_eq!(r.lane_ack_pending(8), 58);
    }

    /// lane provisioning は fail-open（NATS の一時不調で enqueue を止めない）。
    #[test]
    fn tenant_lane_fail_policy_is_open() {
        assert_eq!(
            crate::store::FailPolicy::TENANT_LANE,
            crate::store::FailPolicy::Open
        );
    }
}
