use async_trait::async_trait;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod transport_tests;
#[cfg(test)]
pub use test_support::{FailingStore, InProcStore};

/// ストア操作のエラー。
///
/// [`StoreError::Unavailable`] は「バックエンド到達不能（接続断・タイムアウト等）」を表し、
/// fail-mode 分類（§8）の分岐に使う。それ以外（Lua の戻り値が想定外等）は [`StoreError::Backend`]。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store unavailable: {0}")]
    Unavailable(String),
    /// バックエンドは応答したが想定外の結果／プロトコル不整合。
    #[error("store backend error: {0}")]
    Backend(String),
}

impl StoreError {
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

/// token-bucket のパラメータ（per-tenant `invoke_rate`, §8）。
#[derive(Debug, Clone, Copy)]
pub struct RateLimitParams {
    /// 補充レート（トークン / 秒）= 定常 `invoke_rate`。
    pub refill_per_sec: f64,
    /// バケット容量（バースト上限）。通常は `refill_per_sec` と同等以上。
    pub capacity: f64,
}

/// 共有ストア契約（M3d, §8）。
///
/// invoke admission（レート制限）と OIDC stateを集約する。RedisStore が
/// 本実装、`InProcStore` がテスト用スタブ、`FailingStore` が fail-mode 検証用。
///
/// 状態更新は原子的に行う。枠の消費とstateの取得は冪等ではなく、自動再送しない。
#[async_trait]
pub trait Store: Send + Sync {
    /// token-bucket からトークンを 1 つ消費しようとする（per-tenant レート制限, §8）。
    ///
    /// ストア自身の時刻で補充し、拒否時は `Retry-After`（秒）を返す。
    /// 呼び出し元の時計やリクエストの到着順で補充量を増やしてはならない。
    async fn rate_limit(
        &self,
        tenant: &str,
        params: RateLimitParams,
    ) -> Result<RateDecision, StoreError>;

    async fn ping(&self) -> Result<(), StoreError>;

    /// Short-lived login state; never overwrite a live key. Consumption is atomic
    /// across replicas. No in-process fallback is allowed when Redis is unavailable.
    async fn put_auth_state(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        let _ = (key, value, ttl_secs);
        Err(StoreError::Unavailable(
            "auth state store unavailable".into(),
        ))
    }
    async fn take_auth_state(&self, key: &str) -> Result<Option<String>, StoreError> {
        let _ = key;
        Err(StoreError::Unavailable(
            "auth state store unavailable".into(),
        ))
    }
}

// ============================================================================
// Redis implementation
// ============================================================================

pub use redis_impl::RedisStore;

pub(crate) fn redis_client(url: &str) -> redis::RedisResult<redis::Client> {
    // Other HTTP clients can enable another Rustls provider. Redis uses the
    // process default, so feature-based automatic selection is ambiguous.
    // install_default is thread-safe; an already installed provider is retained.
    let _ = rustls::crypto::ring::default_provider().install_default();
    redis::Client::open(url)
}

mod redis_impl {
    use super::*;
    use redis::aio::{ConnectionManager, ConnectionManagerConfig};
    use redis::{RedisError, Script};

    /// Redis バックエンドの [`Store`] 実装（§8）。
    ///
    /// `ConnectionManager` は自動再接続するマルチプレクス接続（pure-Rust, tokio-comp）。
    /// レート制限は `Script`（EVALSHA）で原子実行し TOCTOU を避ける。
    ///
    /// キーレイアウト（テナント境界をキー空間で明示）:
    /// - レート: `rl:{tenant}` （HASH: tokens, last_ms）
    #[derive(Clone)]
    pub struct RedisStore {
        conn: ConnectionManager,
        rate_script: Script,
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
        /// `redis://` または証明書を検証する `rediss://` で `Store` を構築する。
        ///
        /// 接続失敗は [`StoreError::Unavailable`]。Lua スクリプトはここでコンパイル（ロードは遅延）。
        pub async fn connect(url: &str) -> Result<Self, StoreError> {
            let client = redis_client(url)
                .map_err(|e| StoreError::Unavailable(format!("invalid REDIS_URL: {e}")))?;
            // Bounded fail-closed admission during an unreachable primary. The
            // connection manager reconnects; application writes are never replayed.
            let config = ConnectionManagerConfig::new()
                .set_connection_timeout(std::time::Duration::from_secs(2))
                .set_response_timeout(std::time::Duration::from_secs(2))
                .set_number_of_retries(2)
                .set_max_delay(500);
            let conn = ConnectionManager::new_with_config(client, config)
                .await
                .map_err(map_err)?;
            Ok(Self {
                conn,
                rate_script: Script::new(RATE_LUA),
            })
        }
    }

    // --- Lua スクリプト（すべて単一往復・サーバ側原子実行） ---

    /// token-bucket: `KEYS[1]`=hash key, `ARGV`=[refill_per_sec, capacity]。
    /// 戻り値 `[allowed(0/1), retry_after_secs]`。
    /// RedisのTIMEを使い、時計が戻っても保存済みの補充時刻を巻き戻さない。
    const RATE_LUA: &str = r#"
local clock = redis.call('TIME')
local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)
local rate = tonumber(ARGV[1])
local cap = tonumber(ARGV[2])
local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens'))
local last = tonumber(redis.call('HGET', KEYS[1], 'last'))
if tokens == nil then tokens = cap end
if last == nil then last = now end
now = math.max(now, last)
local elapsed = now - last
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

    fn rate_key(tenant: &str) -> String {
        format!("rl:{tenant}")
    }
    #[async_trait]
    impl Store for RedisStore {
        async fn put_auth_state(
            &self,
            key: &str,
            value: &str,
            ttl_secs: u64,
        ) -> Result<(), StoreError> {
            let result: Option<String> = redis::cmd("SET")
                .arg(format!("auth:{key}"))
                .arg(value)
                .arg("EX")
                .arg(ttl_secs)
                .arg("NX")
                .query_async(&mut self.conn.clone())
                .await
                .map_err(map_err)?;
            match result.as_deref() {
                Some("OK") => Ok(()),
                _ => Err(StoreError::Backend("auth state collision".into())),
            }
        }

        async fn take_auth_state(&self, key: &str) -> Result<Option<String>, StoreError> {
            redis::cmd("GETDEL")
                .arg(format!("auth:{key}"))
                .query_async(&mut self.conn.clone())
                .await
                .map_err(map_err)
        }

        async fn rate_limit(
            &self,
            tenant: &str,
            params: RateLimitParams,
        ) -> Result<RateDecision, StoreError> {
            let mut conn = self.conn.clone();
            let res: (i64, i64) = self
                .rate_script
                .key(rate_key(tenant))
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
    #[tokio::test(start_paused = true)]
    async fn token_bucket_consumes_capacity_then_denies() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0);
        // 容量 5 ぶんは許可。
        for i in 0..5 {
            let d = s.rate_limit("t", p).await.unwrap();
            assert!(d.allowed, "token {i} should be allowed");
            assert_eq!(d.retry_after_secs, 0);
        }
        // 6 個目は枯渇 → 拒否 + Retry-After >= 1。
        let d = s.rate_limit("t", p).await.unwrap();
        assert!(!d.allowed);
        assert!(d.retry_after_secs >= 1);
    }

    /// token-bucket: 時間経過でトークンが補充される（refill）。
    #[tokio::test(start_paused = true)]
    async fn token_bucket_refills_over_time() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0);
        // 容量を使い切る。
        for _ in 0..5 {
            assert!(s.rate_limit("t", p).await.unwrap().allowed);
        }
        assert!(!s.rate_limit("t", p).await.unwrap().allowed);
        // 200ms 経過 → 10/s で 2 トークン補充されるはず。
        tokio::time::advance(std::time::Duration::from_millis(200)).await;
        assert!(s.rate_limit("t", p).await.unwrap().allowed);
        assert!(s.rate_limit("t", p).await.unwrap().allowed);
        // 3 個目は再び枯渇。
        assert!(!s.rate_limit("t", p).await.unwrap().allowed);
    }

    /// token-bucket: 補充は容量で頭打ち（長時間アイドルでも capacity 超にならない）。
    #[tokio::test(start_paused = true)]
    async fn token_bucket_caps_at_capacity() {
        let s = InProcStore::new();
        let p = rl(10.0, 5.0);
        // 1 つ消費して残 4 → 1 時間アイドル後でも最大 5 までしか貯まらない。
        assert!(s.rate_limit("t", p).await.unwrap().allowed);
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        for _ in 0..5 {
            assert!(s.rate_limit("t", p).await.unwrap().allowed);
        }
        assert!(!s.rate_limit("t", p).await.unwrap().allowed);
    }

    /// token-bucket: テナントごとにバケットが独立している。
    #[tokio::test(start_paused = true)]
    async fn token_bucket_is_per_tenant() {
        let s = InProcStore::new();
        let p = rl(1.0, 1.0);
        assert!(s.rate_limit("a", p).await.unwrap().allowed);
        assert!(!s.rate_limit("a", p).await.unwrap().allowed);
        // 別テナントは別バケット → 許可。
        assert!(s.rate_limit("b", p).await.unwrap().allowed);
    }

    /// fail-mode 分類: FailingStore は常に Unavailable を返し、is_unavailable で識別できる。
    #[tokio::test(start_paused = true)]
    async fn failing_store_reports_unavailable() {
        let s = FailingStore;
        let rp = rl(50.0, 50.0);
        let err = s.rate_limit("t", rp).await.unwrap_err();
        assert!(err.is_unavailable());
    }
}
