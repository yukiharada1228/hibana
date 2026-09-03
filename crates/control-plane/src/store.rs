//! 共有ストア (M3d, §8) — 全 Axum インスタンスで共有する低レイテンシ admission 制御。
//!
//! 仕様書 §8 の MUST: Axum はステートレス x N であるため、レート制限・同時実行カウント・
//! login 失敗カウント（§6.0）をインスタンスローカルに持つと上限が実効 N 倍に緩む。これらは
//! **全インスタンスで共有する低レイテンシストア（Redis）** で集計しなければならない。
//!
//! このモジュールは 3 つの admission プリミティブを 1 つの [`Store`] trait に集約する:
//! 1. token-bucket レート制限（per-tenant `invoke_rate`, §8）。
//! 2. in-flight 同時実行 reserve（atomic INCR-cmp-condDECR, §8）+ DECR / reaper 再同期。
//! 3. login 失敗ロックアウト（per (tenant,email) + per IP の両キー, §6.0）。
//!    — M3a の `LoginThrottle` 契約（M3d で本 trait に吸収・一般化し、login.rs は本 Store を使う）。
//!
//! TOCTOU 回避 (MUST): N インスタンスが同じ低カウントを読んで全員 admit する競合を避けるため、
//! reserve / 消費は **単一往復の Lua スクリプト**（read-modify-write をサーバ側で原子実行）で行う。
//!
//! fail-mode 分類 (MUST, §8): admission の各クラスごとに [`FailPolicy`] を明示する。
//! - login ロックアウト: **fail-closed**（Redis 到達不能 → 拒否）。ブルートフォース素通し防止。
//! - invoke レート制限 / in-flight: **fail-open でよい**（Redis 到達不能 → 許可）が、縮退を
//!   ログ・メトリクス・監査（§3.7）に必ず記録する。誤分類は重大インシデント。
//!   個々の判定経路（invoke handler）は [`StoreError::is_unavailable`] を見てクラス別ポリシーを適用する。
//!
//! このモジュールは Redis / DB に触れない **in-proc スタブ**（[`InProcStore`]）と、常に到達不能を
//! 返す [`FailingStore`] を提供し、admission ロジックを live Redis 無しでユニットテストする。
//!
//! 注: 本フェーズ（M3d 共有ストア土台）は trait + 実装 + ユニットテストまでを置く。実際の
//! 呼び出し配線（invoke の rate/in-flight 判定、login の lockout、reaper）は後続フェーズで行う。
//! そのため API 表面の一部はバイナリからまだ呼ばれず dead-code 警告になる。配線は次フェーズで
//! 入るため、ここではモジュール限定で許可する（テストでは大半を実際に行使している）。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

/// admission 制御クラスごとの障害時方針 (§8 MUST)。
///
/// クラスを明示的に型で表現することで、`invoke` のレート制限を誤って fail-closed にして
/// 可用性を落とす／login ロックアウトを誤って fail-open にしてブルートフォースを素通しさせる、
/// といった誤分類を呼び出し側コードでレビューしやすくする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPolicy {
    /// セキュリティ制御。ストア到達不能 → **拒否**（login ロックアウト）。
    Closed,
    /// 可用性優先の性能制御。ストア到達不能 → **許可**（ただし縮退を必ず記録する）。
    Open,
}

impl FailPolicy {
    /// login 失敗ロックアウトの方針（§8: fail-closed）。
    pub const LOGIN_LOCKOUT: FailPolicy = FailPolicy::Closed;
    /// invoke レート制限の方針（§8: fail-open + 縮退記録）。
    pub const INVOKE_RATE: FailPolicy = FailPolicy::Open;
    /// in-flight 同時実行の方針（§8: fail-open + 縮退記録）。
    pub const INFLIGHT: FailPolicy = FailPolicy::Open;
    /// M8 (§7): テナント lane の provisioning の方針（fail-open + 縮退記録）。
    ///
    /// lane を作れない / 状態を読めないときに **enqueue を止めない**（可用性優先）。
    /// 止めると「NATS の一時的な不調でテナントのジョブが一切受け付けられない」ことになり、
    /// invoke_rate / in-flight と同じ可用性クラスの判断に従う。
    /// 縮退したことは必ずログ・監査へ記録する（§8 の「fail-open は許可するが必ず記録する」）。
    pub const TENANT_LANE: FailPolicy = FailPolicy::Open;
}

/// ストア操作のエラー。
///
/// [`StoreError::Unavailable`] は「バックエンド到達不能（接続断・タイムアウト等）」を表し、
/// fail-mode 分類（§8）の分岐に使う。それ以外（Lua の戻り値が想定外等）は [`StoreError::Backend`]。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// バックエンドへ到達できない（接続不可・タイムアウト・IO）。fail-open/closed の分岐対象。
    #[error("store unavailable: {0}")]
    Unavailable(String),
    /// バックエンドは応答したが想定外の結果／プロトコル不整合。
    #[error("store backend error: {0}")]
    Backend(String),
}

impl StoreError {
    /// バックエンド到達不能か（fail-mode 分類で fail-open/closed を選ぶための判定）。
    pub fn is_unavailable(&self) -> bool {
        matches!(self, StoreError::Unavailable(_))
    }
}

/// token-bucket レート制限の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// 許可されたか（トークンを 1 つ消費できたか）。
    pub allowed: bool,
    /// 拒否時の `Retry-After`（秒）。許可時は 0。次の 1 トークンが貯まるまでの秒数。
    pub retry_after_secs: u64,
}

/// in-flight reserve の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveDecision {
    /// 予約できたか（カウンタを上限内で +1 できたか）。false なら 429。
    pub admitted: bool,
    /// 予約後（または拒否時の現在）の in-flight カウント。観測・デバッグ用。
    pub current: i64,
}

/// login ロックアウトの 1 キー分の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutDecision {
    /// ロックアウト中か（閾値到達 → このキーでは拒否すべき）。
    pub locked: bool,
    /// 現在の失敗回数（観測用）。
    pub failures: u64,
}

/// token-bucket のパラメータ（per-tenant `invoke_rate`, §8）。
#[derive(Debug, Clone, Copy)]
pub struct RateLimitParams {
    /// 補充レート（トークン / 秒）= 定常 `invoke_rate`。
    pub refill_per_sec: f64,
    /// バケット容量（バースト上限）。通常は `refill_per_sec` と同等以上。
    pub capacity: f64,
}

/// in-flight 予約のパラメータ。
#[derive(Debug, Clone, Copy)]
pub struct InflightParams {
    /// テナントごとの in-flight 上限（`max_concurrent_executions`, §8）。
    pub max: i64,
    /// カウンタキーの TTL（秒）。reaper が再同期する安全網の一方で、孤立カウンタが
    /// 永久に残らないよう保険として張る（reaper は別途 DB COUNT を真実として上書きする）。
    pub ttl_secs: u64,
}

/// login ロックアウトのパラメータ（§6.0）。
#[derive(Debug, Clone, Copy)]
pub struct LockoutParams {
    /// このキーの失敗閾値（到達でロックアウト）。
    pub threshold: u64,
    /// 失敗カウンタの減衰窓（秒）。最後の失敗から `window_secs` 無失敗で解放される。
    pub window_secs: u64,
}

/// 共有ストア契約（M3d, §8）。
///
/// invoke admission（レート制限 + in-flight）と login ロックアウトを集約する。RedisStore が
/// 本実装、[`InProcStore`] がテスト用スタブ、[`FailingStore`] が fail-mode 検証用。
///
/// すべての操作は冪等／原子的であること（同一往復で read-modify-write）。
#[async_trait]
pub trait Store: Send + Sync {
    /// token-bucket からトークンを 1 つ消費しようとする（per-tenant レート制限, §8）。
    ///
    /// `now_ms` は呼び出し側（Rust）の壁時計（ms）。バケットの最終補充時刻からの経過で
    /// トークンを補充してから 1 つ消費を試みる。拒否時は `Retry-After`（秒）を返す。
    async fn rate_limit(
        &self,
        tenant: &str,
        params: RateLimitParams,
        now_ms: u64,
    ) -> Result<RateDecision, StoreError>;

    /// in-flight を原子的に予約する（INCR-then-compare-then-condDECR を 1 往復で, §8）。
    ///
    /// 上限超過なら +1 を巻き戻して `admitted=false` を返す（TOCTOU 無し）。
    async fn reserve_inflight(
        &self,
        tenant: &str,
        params: InflightParams,
    ) -> Result<ReserveDecision, StoreError>;

    /// in-flight を 1 つ解放する（終端化時に呼ぶ, §8）。
    ///
    /// 0 を下回らないよう floor する（二重 DECR の自己治癒。reaper が真実へ再同期する）。
    async fn release_inflight(&self, tenant: &str) -> Result<i64, StoreError>;

    /// in-flight カウンタを DB COUNT（真実）へ原子的に再同期する（reaper, §8）。
    ///
    /// `count` は `SELECT COUNT(*) FROM executions WHERE status IN ('pending','running')`。
    async fn resync_inflight(
        &self,
        tenant: &str,
        count: i64,
        ttl_secs: u64,
    ) -> Result<(), StoreError>;

    /// login 失敗を 1 件記録し、当該キーの現在状態を返す（§6.0）。
    ///
    /// `key` は呼び出し側が組み立てた識別子（例: `tenant\0email` または `ip:1.2.3.4`）。
    /// 失敗カウンタを +1 して窓 TTL を貼り直し、閾値到達なら `locked=true`。
    async fn record_login_failure(
        &self,
        key: &str,
        params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError>;

    /// login 成功でキーの失敗カウンタをリセットする（§6.0）。
    async fn clear_login_failures(&self, key: &str) -> Result<(), StoreError>;

    /// login 試行前のロックアウト判定（カウントは増やさない, §6.0）。
    ///
    /// 失敗カウンタが閾値以上なら `locked=true`。fail-closed の呼び出し側は `Err`（到達不能）も拒否扱い。
    async fn check_login_lockout(
        &self,
        key: &str,
        params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError>;

    /// readiness probe（M4a, §3.8）。バックエンドへの「ごく軽い」疎通確認のみを行う。
    ///
    /// `/readyz` が DB / NATS / Store の 3 つを並べて 503 を fail-closed で返すための呼び出し。
    /// admission のロジックを実行しない（カウンタを動かさない）こと。`Ok(())` は到達確認のみを意味し、
    /// 整合性の保証を含まない。Redis 到達不能は [`StoreError::Unavailable`] を返す。
    /// `DegradedStore` は常に `Ok(())` を返す（縮退ながら in-proc で機能している = ready 扱い。
    /// 起動時に Redis 不通だったことは main.rs が warn で記録済み）。
    async fn ping(&self) -> Result<(), StoreError>;
}

/// 現在の壁時計（UNIX ms）。token-bucket の `now_ms` 引数に使うヘルパ。
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ============================================================================
// in-proc スタブ（DB-free / Redis-free ユニットテスト用）
// ============================================================================

/// プロセス内の決定論的スタブ実装（テスト・ローカル単一インスタンス用）。
///
/// **注意**: これはプロセスローカルであり、複数 Axum インスタンス間で共有されない。
/// 本番の分散カウンタ要件（§8 MUST）は満たさない。admission ロジック（counter 数学・
/// token-bucket 補充・閾値判定）を live Redis 無しで検証するためのもの。
#[derive(Default)]
pub struct InProcStore {
    inflight: Mutex<HashMap<String, i64>>,
    buckets: Mutex<HashMap<String, Bucket>>,
    failures: Mutex<HashMap<String, Failures>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    /// 残トークン数。
    tokens: f64,
    /// 最終補充時刻（ms）。
    last_ms: u64,
}

#[derive(Clone, Copy)]
struct Failures {
    count: u64,
    /// 最終失敗時刻（ms）。窓減衰判定に使う。
    last_ms: u64,
}

impl InProcStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// テスト用: 現在の in-flight カウントを覗く。
    #[cfg(test)]
    pub fn peek_inflight(&self, tenant: &str) -> i64 {
        *self.inflight.lock().unwrap().get(tenant).unwrap_or(&0)
    }
}

#[async_trait]
impl Store for InProcStore {
    async fn rate_limit(
        &self,
        tenant: &str,
        params: RateLimitParams,
        now_ms: u64,
    ) -> Result<RateDecision, StoreError> {
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(tenant.to_string()).or_insert(Bucket {
            tokens: params.capacity,
            last_ms: now_ms,
        });
        // 経過時間ぶんトークンを補充（容量で頭打ち）。now_ms < last_ms（時計巻き戻り）は 0 経過扱い。
        let elapsed_ms = now_ms.saturating_sub(bucket.last_ms);
        let refilled = (elapsed_ms as f64) / 1000.0 * params.refill_per_sec;
        bucket.tokens = (bucket.tokens + refilled).min(params.capacity);
        bucket.last_ms = now_ms;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(RateDecision {
                allowed: true,
                retry_after_secs: 0,
            })
        } else {
            // 次の 1 トークンまでの時間（秒, 切り上げ・最低 1）。
            let deficit = 1.0 - bucket.tokens;
            let secs = if params.refill_per_sec > 0.0 {
                (deficit / params.refill_per_sec).ceil() as u64
            } else {
                u64::MAX
            };
            Ok(RateDecision {
                allowed: false,
                retry_after_secs: secs.max(1),
            })
        }
    }

    async fn reserve_inflight(
        &self,
        tenant: &str,
        params: InflightParams,
    ) -> Result<ReserveDecision, StoreError> {
        let mut map = self.inflight.lock().unwrap();
        let entry = map.entry(tenant.to_string()).or_insert(0);
        if *entry >= params.max {
            Ok(ReserveDecision {
                admitted: false,
                current: *entry,
            })
        } else {
            *entry += 1;
            Ok(ReserveDecision {
                admitted: true,
                current: *entry,
            })
        }
    }

    async fn release_inflight(&self, tenant: &str) -> Result<i64, StoreError> {
        let mut map = self.inflight.lock().unwrap();
        let entry = map.entry(tenant.to_string()).or_insert(0);
        // 0 を下回らない（二重 DECR の自己治癒）。
        *entry = (*entry - 1).max(0);
        Ok(*entry)
    }

    async fn resync_inflight(
        &self,
        tenant: &str,
        count: i64,
        _ttl_secs: u64,
    ) -> Result<(), StoreError> {
        let mut map = self.inflight.lock().unwrap();
        map.insert(tenant.to_string(), count.max(0));
        Ok(())
    }

    async fn record_login_failure(
        &self,
        key: &str,
        params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        let now = now_unix_millis();
        let mut map = self.failures.lock().unwrap();
        let f = map.entry(key.to_string()).or_insert(Failures {
            count: 0,
            last_ms: now,
        });
        // 窓を過ぎていたらリセットしてから加算（最終失敗からの減衰窓）。
        if now.saturating_sub(f.last_ms) >= params.window_secs.saturating_mul(1000) {
            f.count = 0;
        }
        f.count += 1;
        f.last_ms = now;
        Ok(LockoutDecision {
            locked: f.count >= params.threshold,
            failures: f.count,
        })
    }

    async fn clear_login_failures(&self, key: &str) -> Result<(), StoreError> {
        self.failures.lock().unwrap().remove(key);
        Ok(())
    }

    async fn check_login_lockout(
        &self,
        key: &str,
        params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        let now = now_unix_millis();
        let map = self.failures.lock().unwrap();
        match map.get(key) {
            Some(f) if now.saturating_sub(f.last_ms) < params.window_secs.saturating_mul(1000) => {
                Ok(LockoutDecision {
                    locked: f.count >= params.threshold,
                    failures: f.count,
                })
            }
            // 窓を過ぎている or 記録なし → 失敗 0 扱い。
            _ => Ok(LockoutDecision {
                locked: false,
                failures: 0,
            }),
        }
    }

    async fn ping(&self) -> Result<(), StoreError> {
        // in-proc は常に到達可能（テスト用スタブ）。
        Ok(())
    }
}

// ============================================================================
// 常に到達不能を返すスタブ（fail-mode 分類の検証用）
// ============================================================================

/// 全操作が [`StoreError::Unavailable`] を返すスタブ。
///
/// fail-mode 分類のテスト用: login ロックアウト経路が fail-closed（拒否）に、invoke 経路が
/// fail-open（許可 + 縮退記録）になることを呼び出し側で検証するために使う。
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingStore;

impl FailingStore {
    fn down<T>() -> Result<T, StoreError> {
        Err(StoreError::Unavailable("failing store (test)".to_string()))
    }
}

#[async_trait]
impl Store for FailingStore {
    async fn rate_limit(
        &self,
        _tenant: &str,
        _params: RateLimitParams,
        _now_ms: u64,
    ) -> Result<RateDecision, StoreError> {
        Self::down()
    }
    async fn reserve_inflight(
        &self,
        _tenant: &str,
        _params: InflightParams,
    ) -> Result<ReserveDecision, StoreError> {
        Self::down()
    }
    async fn release_inflight(&self, _tenant: &str) -> Result<i64, StoreError> {
        Self::down()
    }
    async fn resync_inflight(
        &self,
        _tenant: &str,
        _count: i64,
        _ttl_secs: u64,
    ) -> Result<(), StoreError> {
        Self::down()
    }
    async fn record_login_failure(
        &self,
        _key: &str,
        _params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        Self::down()
    }
    async fn clear_login_failures(&self, _key: &str) -> Result<(), StoreError> {
        Self::down()
    }
    async fn check_login_lockout(
        &self,
        _key: &str,
        _params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        Self::down()
    }
    async fn ping(&self) -> Result<(), StoreError> {
        Self::down()
    }
}

// ============================================================================
// 起動時 Redis 不通フォールバック用の縮退ストア（fail-mode を壊さない）
// ============================================================================

/// 起動時に Redis が到達不能だったときの **縮退フォールバック**ストア（§8 fail-mode 保全）。
///
/// 素の [`InProcStore`] にフォールバックすると致命的な誤分類が起きる: InProcStore は決して
/// `Err` を返さないため、login ロックアウトの fail-CLOSED 判定（`check_login_lockout` の
/// `Unavailable` 経路）が **プロセス寿命の間ずっと発火できなくなり**、ブルートフォースが
/// 素通しする（login が事実上 fail-OPEN に反転する）。これは §8 が禁じる「セキュリティ制御の
/// 黙示的喪失」そのものである。
///
/// この adapter はクラス別 fail-mode を保つ:
/// - **login ロックアウト系**（`record_login_failure` / `clear_login_failures` /
///   `check_login_lockout`）= [`FailPolicy::Closed`] → 常に [`StoreError::Unavailable`] を返す。
///   呼び出し側（login.rs）は `Err` を `LockoutCheck::Unavailable` にし fail-CLOSED で拒否する。
/// - **invoke 系**（rate / in-flight / reaper resync）= [`FailPolicy::Open`] → 内部の
///   [`InProcStore`] に委譲して許可する（ただしインスタンスローカルで分散共有ではない;
///   選択時に main.rs が縮退を warn で記録する。§8 の縮退記録要件）。
///
/// **注意**: これはあくまで「Redis がブート時に不通」のときの可用性維持用。invoke カウンタは
/// インスタンスローカルになり上限が実効 N 倍に緩むため、本番では Redis を堅牢化すること。
pub struct DegradedStore {
    inner: InProcStore,
}

impl DegradedStore {
    pub fn new() -> Self {
        Self {
            inner: InProcStore::new(),
        }
    }

    fn login_unavailable<T>() -> Result<T, StoreError> {
        Err(StoreError::Unavailable(
            "shared store unreachable at startup; login lockout fails CLOSED (degraded store)"
                .to_string(),
        ))
    }
}

impl Default for DegradedStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Store for DegradedStore {
    // --- invoke 系: fail-open（in-proc に委譲して許可する） ---
    async fn rate_limit(
        &self,
        tenant: &str,
        params: RateLimitParams,
        now_ms: u64,
    ) -> Result<RateDecision, StoreError> {
        self.inner.rate_limit(tenant, params, now_ms).await
    }
    async fn reserve_inflight(
        &self,
        tenant: &str,
        params: InflightParams,
    ) -> Result<ReserveDecision, StoreError> {
        self.inner.reserve_inflight(tenant, params).await
    }
    async fn release_inflight(&self, tenant: &str) -> Result<i64, StoreError> {
        self.inner.release_inflight(tenant).await
    }
    async fn resync_inflight(
        &self,
        tenant: &str,
        count: i64,
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        self.inner.resync_inflight(tenant, count, ttl_secs).await
    }

    // --- login ロックアウト系: fail-CLOSED（常に Unavailable → 呼び出し側で拒否） ---
    async fn record_login_failure(
        &self,
        _key: &str,
        _params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        Self::login_unavailable()
    }
    async fn clear_login_failures(&self, _key: &str) -> Result<(), StoreError> {
        Self::login_unavailable()
    }
    async fn check_login_lockout(
        &self,
        _key: &str,
        _params: LockoutParams,
    ) -> Result<LockoutDecision, StoreError> {
        Self::login_unavailable()
    }

    /// readiness は **Ok** を返す（M4a, §3.8）。
    ///
    /// DegradedStore は「起動時に Redis に届かなかったが in-proc 縮退で稼働中」の状態であり、
    /// プロセスとしてはリクエストを処理できる（invoke 系 fail-open / login 系 fail-closed）。
    /// `/readyz` をここで 503 にすると、Redis 一時障害でクラスタ全体が NotReady になり、
    /// liveness probe との分離（§3.8）が崩れる。`store.ping()` は到達確認用 hop に絞り、
    /// 縮退状態は main.rs の起動 warn で運用が把握する責務とする。
    async fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

// ============================================================================
// Redis 実装（ConnectionManager + Lua スクリプト）
// ============================================================================

pub use redis_impl::RedisStore;

mod redis_impl {
    use super::*;
    use redis::aio::ConnectionManager;
    use redis::{RedisError, Script};

    /// Redis バックエンドの [`Store`] 実装（§8）。
    ///
    /// `ConnectionManager` は自動再接続するマルチプレクス接続（pure-Rust, tokio-comp）。
    /// 全 read-modify-write は `Script`（EVALSHA, 単一往復）で原子実行し TOCTOU を避ける。
    ///
    /// キーレイアウト（テナント境界をキー空間で明示）:
    /// - レート: `rl:{tenant}` （HASH: tokens, last_ms）
    /// - in-flight: `inflight:{tenant}` （INTEGER）
    /// - ロックアウト: `lockout:{key}` （INTEGER + TTL）
    #[derive(Clone)]
    pub struct RedisStore {
        conn: ConnectionManager,
        rate_script: Script,
        reserve_script: Script,
        lockout_incr_script: Script,
    }

    /// RedisError → StoreError。IO / 接続/ タイムアウト系は Unavailable（fail-mode 分岐対象）、
    /// それ以外は Backend に分類する。
    fn map_err(e: RedisError) -> StoreError {
        if e.is_connection_dropped()
            || e.is_connection_refusal()
            || e.is_timeout()
            || e.is_io_error()
        {
            StoreError::Unavailable(e.to_string())
        } else {
            StoreError::Backend(e.to_string())
        }
    }

    impl RedisStore {
        /// Redis URL（例: `redis://127.0.0.1:6379`）へ接続して `Store` を構築する。
        ///
        /// 接続失敗は [`StoreError::Unavailable`]。Lua スクリプトはここでコンパイル（ロードは遅延）。
        pub async fn connect(url: &str) -> Result<Self, StoreError> {
            let client = redis::Client::open(url)
                .map_err(|e| StoreError::Unavailable(format!("invalid REDIS_URL: {e}")))?;
            let conn = ConnectionManager::new(client).await.map_err(map_err)?;
            Ok(Self {
                conn,
                rate_script: Script::new(RATE_LUA),
                reserve_script: Script::new(RESERVE_LUA),
                lockout_incr_script: Script::new(LOCKOUT_INCR_LUA),
            })
        }
    }

    // --- Lua スクリプト（すべて単一往復・サーバ側原子実行） ---

    /// token-bucket: `KEYS[1]`=hash key, `ARGV`=[now_ms, refill_per_sec, capacity]。
    /// 戻り値 `[allowed(0/1), retry_after_secs]`。
    /// 経過時間ぶん補充 → 1 トークン消費を試行。now が過去より小さい場合は 0 経過。
    const RATE_LUA: &str = r#"
local now = tonumber(ARGV[1])
local rate = tonumber(ARGV[2])
local cap = tonumber(ARGV[3])
local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens'))
local last = tonumber(redis.call('HGET', KEYS[1], 'last'))
if tokens == nil then tokens = cap end
if last == nil then last = now end
local elapsed = now - last
if elapsed < 0 then elapsed = 0 end
tokens = math.min(cap, tokens + (elapsed / 1000.0) * rate)
local allowed = 0
local retry = 0
if tokens >= 1.0 then
  tokens = tokens - 1.0
  allowed = 1
else
  local deficit = 1.0 - tokens
  if rate > 0 then
    retry = math.ceil(deficit / rate)
    if retry < 1 then retry = 1 end
  else
    retry = 2147483647
  end
end
redis.call('HSET', KEYS[1], 'tokens', tokens, 'last', now)
-- バケットが満杯付近で放置されても永久に残らないよう、容量が貯まる十分先の TTL を貼る。
local ttl = math.ceil(cap / math.max(rate, 0.001)) + 60
redis.call('EXPIRE', KEYS[1], ttl)
return {allowed, retry}
"#;

    /// in-flight reserve: `KEYS[1]`=counter key, `ARGV`=[max, ttl_secs]。
    /// INCR → 上限超なら DECR で巻き戻す（原子）。戻り値 `[admitted(0/1), current]`。
    const RESERVE_LUA: &str = r#"
local maxv = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local cur = redis.call('INCR', KEYS[1])
if cur > maxv then
  cur = redis.call('DECR', KEYS[1])
  return {0, cur}
end
if ttl > 0 then redis.call('EXPIRE', KEYS[1], ttl) end
return {1, cur}
"#;

    /// login 失敗 INCR: `KEYS[1]`=counter key, `ARGV`=[threshold, window_secs]。
    /// INCR → 窓 TTL を貼り直す。戻り値 `[locked(0/1), failures]`。
    const LOCKOUT_INCR_LUA: &str = r#"
local threshold = tonumber(ARGV[1])
local window = tonumber(ARGV[2])
local n = redis.call('INCR', KEYS[1])
if window > 0 then redis.call('EXPIRE', KEYS[1], window) end
local locked = 0
if n >= threshold then locked = 1 end
return {locked, n}
"#;

    fn rate_key(tenant: &str) -> String {
        format!("rl:{tenant}")
    }
    fn inflight_key(tenant: &str) -> String {
        format!("inflight:{tenant}")
    }
    fn lockout_key(key: &str) -> String {
        format!("lockout:{key}")
    }

    #[async_trait]
    impl Store for RedisStore {
        async fn rate_limit(
            &self,
            tenant: &str,
            params: RateLimitParams,
            now_ms: u64,
        ) -> Result<RateDecision, StoreError> {
            let mut conn = self.conn.clone();
            let res: (i64, i64) = self
                .rate_script
                .key(rate_key(tenant))
                .arg(now_ms)
                .arg(params.refill_per_sec)
                .arg(params.capacity)
                .invoke_async(&mut conn)
                .await
                .map_err(map_err)?;
            Ok(RateDecision {
                allowed: res.0 == 1,
                retry_after_secs: res.1.max(0) as u64,
            })
        }

        async fn reserve_inflight(
            &self,
            tenant: &str,
            params: InflightParams,
        ) -> Result<ReserveDecision, StoreError> {
            let mut conn = self.conn.clone();
            let res: (i64, i64) = self
                .reserve_script
                .key(inflight_key(tenant))
                .arg(params.max)
                .arg(params.ttl_secs)
                .invoke_async(&mut conn)
                .await
                .map_err(map_err)?;
            Ok(ReserveDecision {
                admitted: res.0 == 1,
                current: res.1,
            })
        }

        async fn release_inflight(&self, tenant: &str) -> Result<i64, StoreError> {
            // DECR して 0 未満なら 0 へ補正（二重 DECR 自己治癒）。2 コマンドだが
            // 単調減少なので競合してもカウンタが負に居座らないことのみ保証すればよい。
            let mut conn = self.conn.clone();
            let key = inflight_key(tenant);
            let cur: i64 = redis::cmd("DECR")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            if cur < 0 {
                let _: () = redis::cmd("SET")
                    .arg(&key)
                    .arg(0)
                    .query_async(&mut conn)
                    .await
                    .map_err(map_err)?;
                return Ok(0);
            }
            Ok(cur)
        }

        async fn resync_inflight(
            &self,
            tenant: &str,
            count: i64,
            ttl_secs: u64,
        ) -> Result<(), StoreError> {
            let mut conn = self.conn.clone();
            let key = inflight_key(tenant);
            let v = count.max(0);
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg(v)
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            if ttl_secs > 0 {
                let _: () = redis::cmd("EXPIRE")
                    .arg(&key)
                    .arg(ttl_secs)
                    .query_async(&mut conn)
                    .await
                    .map_err(map_err)?;
            }
            Ok(())
        }

        async fn record_login_failure(
            &self,
            key: &str,
            params: LockoutParams,
        ) -> Result<LockoutDecision, StoreError> {
            let mut conn = self.conn.clone();
            let res: (i64, i64) = self
                .lockout_incr_script
                .key(lockout_key(key))
                .arg(params.threshold)
                .arg(params.window_secs)
                .invoke_async(&mut conn)
                .await
                .map_err(map_err)?;
            Ok(LockoutDecision {
                locked: res.0 == 1,
                failures: res.1.max(0) as u64,
            })
        }

        async fn clear_login_failures(&self, key: &str) -> Result<(), StoreError> {
            let mut conn = self.conn.clone();
            let _: () = redis::cmd("DEL")
                .arg(lockout_key(key))
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            Ok(())
        }

        async fn check_login_lockout(
            &self,
            key: &str,
            params: LockoutParams,
        ) -> Result<LockoutDecision, StoreError> {
            let mut conn = self.conn.clone();
            // 窓内のキーが存在すれば値を読む。TTL で自然減衰するため GET のみで十分。
            let n: Option<i64> = redis::cmd("GET")
                .arg(lockout_key(key))
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            let failures = n.unwrap_or(0).max(0) as u64;
            Ok(LockoutDecision {
                locked: failures >= params.threshold,
                failures,
            })
        }

        async fn ping(&self) -> Result<(), StoreError> {
            // PING は副作用なし・最軽量の疎通確認（/readyz の Redis hop）。
            // 期待応答は "PONG" だが、接続不能なら map_err 経由で StoreError::Unavailable になり、
            // /readyz が 503 を返す。応答が "PONG" でなくても（プロトコル不整合）Backend として扱い、
            // 呼び出し側（/readyz）はどちらも fail-closed で 503 にする。
            let mut conn = self.conn.clone();
            let pong: String = redis::cmd("PING")
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            if pong.eq_ignore_ascii_case("PONG") {
                Ok(())
            } else {
                Err(StoreError::Backend(format!(
                    "unexpected PING reply: {pong}"
                )))
            }
        }
    }
}

// ============================================================================
// ユニットテスト（DB-free / Redis-free。in-proc スタブに対して admission 数学を検証）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(refill: f64, cap: f64) -> RateLimitParams {
        RateLimitParams {
            refill_per_sec: refill,
            capacity: cap,
        }
    }

    /// token-bucket: 容量ぶんは即時許可、その後は枯渇して 429（Retry-After 付き）。
    #[tokio::test]
    async fn token_bucket_consumes_capacity_then_denies() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0); // 5 burst, 10/s 補充
        let t0 = 1_000_000;
        // 容量 5 ぶんは許可。
        for i in 0..5 {
            let d = s.rate_limit("t", p, t0).await.unwrap();
            assert!(d.allowed, "token {i} should be allowed");
            assert_eq!(d.retry_after_secs, 0);
        }
        // 6 個目は枯渇 → 拒否 + Retry-After >= 1。
        let d = s.rate_limit("t", p, t0).await.unwrap();
        assert!(!d.allowed);
        assert!(d.retry_after_secs >= 1);
    }

    /// token-bucket: 時間経過でトークンが補充される（refill）。
    #[tokio::test]
    async fn token_bucket_refills_over_time() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0);
        let t0 = 1_000_000;
        // 容量を使い切る。
        for _ in 0..5 {
            assert!(s.rate_limit("t", p, t0).await.unwrap().allowed);
        }
        assert!(!s.rate_limit("t", p, t0).await.unwrap().allowed);
        // 200ms 経過 → 10/s で 2 トークン補充されるはず。
        let t1 = t0 + 200;
        assert!(s.rate_limit("t", p, t1).await.unwrap().allowed);
        assert!(s.rate_limit("t", p, t1).await.unwrap().allowed);
        // 3 個目は再び枯渇。
        assert!(!s.rate_limit("t", p, t1).await.unwrap().allowed);
    }

    /// token-bucket: 補充は容量で頭打ち（長時間アイドルでも capacity 超にならない）。
    #[tokio::test]
    async fn token_bucket_caps_at_capacity() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0);
        let t0 = 1_000_000;
        // 1 つ消費して残 4 → 1 時間アイドル後でも最大 5 までしか貯まらない。
        assert!(s.rate_limit("t", p, t0).await.unwrap().allowed);
        let t1 = t0 + 3_600_000;
        for _ in 0..5 {
            assert!(s.rate_limit("t", p, t1).await.unwrap().allowed);
        }
        assert!(!s.rate_limit("t", p, t1).await.unwrap().allowed);
    }

    /// token-bucket: テナントごとにバケットが独立している。
    #[tokio::test]
    async fn token_bucket_is_per_tenant() {
        let s = InProcStore::new();
        let p = rl(1.0, 1.0);
        let t0 = 1_000_000;
        assert!(s.rate_limit("a", p, t0).await.unwrap().allowed);
        assert!(!s.rate_limit("a", p, t0).await.unwrap().allowed);
        // 別テナントは別バケット → 許可。
        assert!(s.rate_limit("b", p, t0).await.unwrap().allowed);
    }

    /// in-flight reserve: 上限まで許可、超過で拒否（current は据え置き）。
    #[tokio::test]
    async fn inflight_reserve_allows_up_to_max_then_denies() {
        let s = InProcStore::new();
        let p = InflightParams {
            max: 3,
            ttl_secs: 60,
        };
        for i in 1..=3 {
            let d = s.reserve_inflight("t", p).await.unwrap();
            assert!(d.admitted);
            assert_eq!(d.current, i);
        }
        let d = s.reserve_inflight("t", p).await.unwrap();
        assert!(!d.admitted, "over max must be denied");
        assert_eq!(d.current, 3, "denied reserve must not increment");
        assert_eq!(s.peek_inflight("t"), 3);
    }

    /// in-flight DECR: 解放で枠が空き、再 reserve できる。
    #[tokio::test]
    async fn inflight_release_frees_a_slot() {
        let s = InProcStore::new();
        let p = InflightParams {
            max: 1,
            ttl_secs: 60,
        };
        assert!(s.reserve_inflight("t", p).await.unwrap().admitted);
        assert!(!s.reserve_inflight("t", p).await.unwrap().admitted);
        // 解放 → 1 枠空く。
        assert_eq!(s.release_inflight("t").await.unwrap(), 0);
        assert!(s.reserve_inflight("t", p).await.unwrap().admitted);
    }

    /// in-flight DECR: 0 を下回らない（二重 DECR の自己治癒）。
    #[tokio::test]
    async fn inflight_release_floors_at_zero() {
        let s = InProcStore::new();
        assert_eq!(s.release_inflight("t").await.unwrap(), 0);
        assert_eq!(s.release_inflight("t").await.unwrap(), 0);
        assert_eq!(s.peek_inflight("t"), 0);
    }

    /// reaper 再同期: カウンタを DB COUNT（真実）へ上書きする。
    #[tokio::test]
    async fn inflight_resync_overwrites_counter() {
        let s = InProcStore::new();
        let p = InflightParams {
            max: 100,
            ttl_secs: 60,
        };
        // ドリフトで膨らんだカウンタ。
        for _ in 0..10 {
            s.reserve_inflight("t", p).await.unwrap();
        }
        assert_eq!(s.peek_inflight("t"), 10);
        // DB の真実は 3 件 → 再同期。
        s.resync_inflight("t", 3, 60).await.unwrap();
        assert_eq!(s.peek_inflight("t"), 3);
    }

    /// login ロックアウト: 閾値到達で locked。それ未満は許可。
    #[tokio::test]
    async fn lockout_triggers_at_threshold() {
        let s = InProcStore::new();
        let p = LockoutParams {
            threshold: 3,
            window_secs: 900,
        };
        // 事前チェックは失敗 0 → 未ロック。
        assert!(!s.check_login_lockout("k", p).await.unwrap().locked);
        let d1 = s.record_login_failure("k", p).await.unwrap();
        assert!(!d1.locked);
        assert_eq!(d1.failures, 1);
        let d2 = s.record_login_failure("k", p).await.unwrap();
        assert!(!d2.locked);
        // 3 回目で閾値到達 → locked。
        let d3 = s.record_login_failure("k", p).await.unwrap();
        assert!(d3.locked);
        assert_eq!(d3.failures, 3);
        // 以後 check も locked。
        assert!(s.check_login_lockout("k", p).await.unwrap().locked);
    }

    /// login ロックアウト: 成功でカウンタがリセットされる。
    #[tokio::test]
    async fn lockout_cleared_on_success() {
        let s = InProcStore::new();
        let p = LockoutParams {
            threshold: 2,
            window_secs: 900,
        };
        s.record_login_failure("k", p).await.unwrap();
        s.record_login_failure("k", p).await.unwrap();
        assert!(s.check_login_lockout("k", p).await.unwrap().locked);
        s.clear_login_failures("k").await.unwrap();
        assert!(!s.check_login_lockout("k", p).await.unwrap().locked);
    }

    /// login ロックアウトは両キーで独立にカウントされる（(tenant,email) と IP）。
    #[tokio::test]
    async fn lockout_keys_are_independent() {
        let s = InProcStore::new();
        let p = LockoutParams {
            threshold: 2,
            window_secs: 900,
        };
        s.record_login_failure("tenant\u{0}user@example.com", p)
            .await
            .unwrap();
        // 別キー（IP）はまだ 0。
        assert!(!s.check_login_lockout("ip:1.2.3.4", p).await.unwrap().locked);
    }

    /// fail-mode 分類: FailingStore は常に Unavailable を返し、is_unavailable で識別できる。
    #[tokio::test]
    async fn failing_store_reports_unavailable() {
        let s = FailingStore;
        let p = LockoutParams {
            threshold: 3,
            window_secs: 900,
        };
        let err = s.check_login_lockout("k", p).await.unwrap_err();
        assert!(err.is_unavailable());

        let rp = rl(50.0, 50.0);
        let err = s.rate_limit("t", rp, 0).await.unwrap_err();
        assert!(err.is_unavailable());
    }

    /// fail-mode ポリシー: login は Closed、invoke 系は Open（誤分類防止の定数を固定）。
    #[test]
    fn fail_policy_classification_is_explicit() {
        assert_eq!(FailPolicy::LOGIN_LOCKOUT, FailPolicy::Closed);
        assert_eq!(FailPolicy::INVOKE_RATE, FailPolicy::Open);
        assert_eq!(FailPolicy::INFLIGHT, FailPolicy::Open);
    }

    /// DegradedStore（起動時 Redis 不通フォールバック）は fail-mode を保つ:
    /// login ロックアウト系は **Unavailable**（→ 呼び出し側 fail-CLOSED で拒否）、
    /// invoke 系は許可（fail-OPEN）。素の InProcStore を使うと login が fail-OPEN に
    /// 反転してブルートフォースが素通しするため、その回帰を固定する。
    #[tokio::test]
    async fn degraded_store_keeps_login_fail_closed_and_invoke_open() {
        let s = DegradedStore::new();
        let lp = LockoutParams {
            threshold: 3,
            window_secs: 900,
        };
        // login ロックアウト系は到達不能（fail-closed）。InProcStore なら Ok を返してしまう。
        assert!(s
            .check_login_lockout("k", lp)
            .await
            .unwrap_err()
            .is_unavailable());
        assert!(s
            .record_login_failure("k", lp)
            .await
            .unwrap_err()
            .is_unavailable());
        assert!(s
            .clear_login_failures("k")
            .await
            .unwrap_err()
            .is_unavailable());

        // invoke 系は許可される（fail-open; インスタンスローカルだが可用性維持）。
        let rp = rl(10.0, 5.0);
        assert!(s.rate_limit("t", rp, 1_000_000).await.unwrap().allowed);
        let ip = InflightParams {
            max: 2,
            ttl_secs: 60,
        };
        assert!(s.reserve_inflight("t", ip).await.unwrap().admitted);
        assert_eq!(s.release_inflight("t").await.unwrap(), 0);
        s.resync_inflight("t", 0, 60).await.unwrap();
    }
}
