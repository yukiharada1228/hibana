//! In-memory and unavailable stores for DB-free regression tests.
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

use super::{RateDecision, RateLimitParams, Store, StoreError};
use async_trait::async_trait;

// ============================================================================
// in-proc スタブ（DB-free / Redis-free ユニットテスト用）
// ============================================================================

/// プロセス内の決定論的スタブ実装（テスト専用）。
///
/// **注意**: これはプロセスローカルであり、複数 Axum インスタンス間で共有されない。
/// 本番の分散カウンタ要件（§8 MUST）は満たさない。admission ロジック（counter 数学・
/// token-bucket 補充・閾値判定）を live Redis 無しで検証するためのもの。
#[derive(Default)]
pub struct InProcStore {
    buckets: Mutex<HashMap<String, Bucket>>,
    auth_states: Mutex<HashMap<String, (String, Instant)>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    /// 残トークン数。
    tokens: f64,
    last: Instant,
}

impl InProcStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InProcStore {
    async fn put_auth_state(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        let mut states = self.auth_states.lock().unwrap();
        let now = Instant::now();
        states.retain(|_, (_, expires)| *expires > now);
        if states.contains_key(key) {
            return Err(StoreError::Backend("auth state collision".into()));
        }
        states.insert(
            key.into(),
            (value.into(), now + Duration::from_secs(ttl_secs)),
        );
        Ok(())
    }

    async fn take_auth_state(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .auth_states
            .lock()
            .unwrap()
            .remove(key)
            .filter(|(_, expires)| *expires > Instant::now())
            .map(|(value, _)| value))
    }

    async fn rate_limit(
        &self,
        tenant: &str,
        params: RateLimitParams,
    ) -> Result<RateDecision, StoreError> {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let bucket = buckets.entry(tenant.to_string()).or_insert(Bucket {
            tokens: params.capacity,
            last: now,
        });
        let refilled = now.duration_since(bucket.last).as_secs_f64() * params.refill_per_sec;
        bucket.tokens = (bucket.tokens + refilled).min(params.capacity);
        bucket.last = now;

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

    async fn ping(&self) -> Result<(), StoreError> {
        // in-proc は常に到達可能（テスト用スタブ）。
        Ok(())
    }
}

// ============================================================================
// 常に到達不能を返すスタブ（fail-mode 分類の検証用）
// ============================================================================

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
    ) -> Result<RateDecision, StoreError> {
        Self::down()
    }
    async fn ping(&self) -> Result<(), StoreError> {
        Self::down()
    }
}
