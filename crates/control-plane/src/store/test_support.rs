//! In-memory and unavailable stores for DB-free regression tests.
use std::collections::HashMap;
use std::sync::Mutex;

use super::{
    now_unix_millis, InflightParams, LockoutDecision, LockoutParams, RateDecision, RateLimitParams,
    ReserveDecision, Store, StoreError,
};
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
