//! Short-lived, bounded live feed shared by all Control Plane replicas.
//! Publication is best effort after DB commit, never a condition of HTTP success.
use redis::{aio::ConnectionManager, Script};
use std::sync::LazyLock;

pub const CAPACITY: usize = 1000;
pub const PAGE_SIZE: usize = 100;
pub const TTL_SECONDS: u64 = 90;

pub struct Page {
    pub execution_ids: Vec<String>,
    pub cursor: String,
    pub lagged: bool,
    pub has_more: bool,
}

/// Notify only after durable completion. One budget covers the whole batch so
/// a Redis outage cannot delay recovery once per execution or fail an invocation.
pub(crate) async fn publish_completed(
    store: &dyn super::Store,
    tenant: &str,
    executions: &[(&str, &str)],
) {
    if !matches!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            for (component, execution) in executions {
                store.publish_tail(tenant, component, execution).await?;
            }
            Ok::<_, super::StoreError>(())
        })
        .await,
        Ok(Ok(()))
    ) {
        tracing::warn!(
            tenant,
            "live tail notifications unavailable; executions remain in stored logs"
        );
    }
}

pub fn valid_cursor(value: &str) -> bool {
    value.len() <= 41 && parts(value).is_some()
}

fn parts(value: &str) -> Option<(u64, u64)> {
    let (a, b) = value.split_once('-')?;
    if a.is_empty() || b.is_empty() || !a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn key(tenant: &str, component: &str) -> String {
    // Length-prefixing prevents ambiguity even for operator-supplied identifiers.
    format!("tail:{}:{tenant}:{component}", tenant.len())
}

pub async fn publish(
    conn: &mut ConnectionManager,
    tenant: &str,
    component: &str,
    execution: &str,
) -> redis::RedisResult<()> {
    // No subscribers: no history is created. Only readers renew the lease.
    // Exact trimming bounds memory, including when clients cannot keep up.
    static SCRIPT: LazyLock<Script> = LazyLock::new(|| {
        Script::new(
            r#"
        if redis.call('EXISTS', KEYS[1]) == 1 then
            redis.call('XADD', KEYS[1], 'MAXLEN', ARGV[2], '*', 'execution', ARGV[1])
        end
        return 1
    "#,
        )
    });
    SCRIPT
        .key(key(tenant, component))
        .arg(execution)
        .arg(CAPACITY)
        .invoke_async::<i64>(conn)
        .await?;
    Ok(())
}

pub async fn read(
    conn: &mut ConnectionManager,
    tenant: &str,
    component: &str,
    cursor: Option<&str>,
) -> redis::RedisResult<Page> {
    // Establishing the boundary and reading are atomic relative to publication.
    // The empty sentinel also detects expiry/reset, including a quiet application.
    type Entry = (String, Vec<String>);
    static SCRIPT: LazyLock<Script> = LazyLock::new(|| {
        Script::new(
            r#"
        local reset = 0
        if redis.call('EXISTS', KEYS[1]) == 0 then
            redis.call('XADD', KEYS[1], '*', 'execution', '')
            reset = 1
        end
        redis.call('EXPIRE', KEYS[1], ARGV[2])
        local first = redis.call('XRANGE', KEYS[1], '-', '+', 'COUNT', 1)[1][1]
        local latest = redis.call('XREVRANGE', KEYS[1], '+', '-', 'COUNT', 1)[1][1]
        local entries = {}
        if ARGV[1] ~= '' and reset == 0 then
            entries = redis.call('XRANGE', KEYS[1], '(' .. ARGV[1], '+', 'COUNT', ARGV[3])
        end
        return {reset, first, latest, entries}
    "#,
        )
    });
    let (reset, first, latest, mut entries): (bool, String, String, Vec<Entry>) = SCRIPT
        .key(key(tenant, component))
        .arg(cursor.unwrap_or_default())
        .arg(TTL_SECONDS)
        .arg(PAGE_SIZE + 1)
        .invoke_async(conn)
        .await?;
    let has_more = entries.len() > PAGE_SIZE;
    entries.truncate(PAGE_SIZE);
    let lagged =
        cursor.is_some_and(|c| reset || parts(c) < parts(&first) || parts(c) > parts(&latest));
    let next = if reset || cursor.is_none() || cursor.is_some_and(|c| parts(c) > parts(&latest)) {
        latest
    } else {
        entries
            .last()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| cursor.unwrap().into())
    };
    Ok(Page {
        execution_ids: entries
            .into_iter()
            .filter_map(|(_, fields)| fields.get(1).cloned().filter(|id| !id.is_empty()))
            .collect(),
        cursor: next,
        lagged,
        has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn completion_notifications_share_one_batch_deadline() {
        use super::super::{RateDecision, RateLimitParams, Store, StoreError};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct SlowStore(AtomicUsize);
        #[async_trait::async_trait]
        impl Store for SlowStore {
            async fn rate_limit(
                &self,
                _: &str,
                _: RateLimitParams,
            ) -> Result<RateDecision, StoreError> {
                unreachable!()
            }
            async fn ping(&self) -> Result<(), StoreError> {
                Ok(())
            }
            async fn publish_tail(&self, _: &str, _: &str, _: &str) -> Result<(), StoreError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                Ok(())
            }
        }
        let store = SlowStore(AtomicUsize::new(0));
        let started = tokio::time::Instant::now();
        publish_completed(
            &store,
            "tenant",
            &[("app", "1"), ("app", "2"), ("app", "3")],
        )
        .await;
        assert_eq!(store.0.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < std::time::Duration::from_millis(120));
    }
    #[test]
    fn cursor_validation_is_bounded_and_numeric() {
        for value in ["0-0", "123-45", "18446744073709551615-18446744073709551615"] {
            assert!(valid_cursor(value));
        }
        for value in [
            "",
            "-",
            "+1-0",
            "1--1",
            "1-0\n",
            "1-2-3",
            "18446744073709551616-0",
        ] {
            assert!(!valid_cursor(value));
        }
    }

    #[tokio::test]
    #[ignore = "requires scripts/test-redis-tls.sh"]
    async fn redis_transport_live_tail_is_shared_bounded_and_reports_gaps() {
        use crate::store::{RedisStore, Store};
        use redis::AsyncCommands;
        for endpoint in ["HIBANA_TEST_REDIS_TCP_URL", "HIBANA_TEST_REDIS_TLS_URL"] {
            let url = std::env::var(endpoint).unwrap();
            let parsed = reqwest::Url::parse(&url).unwrap();
            assert!(matches!(parsed.host_str(), Some("localhost" | "127.0.0.1")));
            let reader = RedisStore::connect(&url).await.unwrap();
            let publisher = RedisStore::connect(&url).await.unwrap();
            let tenant = hibana_shared::new_execution_id();
            let mut conn = crate::store::redis_client(&url)
                .unwrap()
                .get_multiplexed_async_connection()
                .await
                .unwrap();
            publisher
                .publish_tail(&tenant, "app", "before-subscribing")
                .await
                .unwrap();
            assert!(!conn.exists::<_, bool>(key(&tenant, "app")).await.unwrap());
            let start = reader.read_tail(&tenant, "app", None).await.unwrap();
            assert!(start.execution_ids.is_empty());
            assert!(!start.lagged);
            publisher
                .publish_tail(&tenant, "app", "exec-new")
                .await
                .unwrap();
            publisher
                .publish_tail(&tenant, "app", "exec-long-running")
                .await
                .unwrap();
            let page = reader
                .read_tail(&tenant, "app", Some(&start.cursor))
                .await
                .unwrap();
            assert_eq!(page.execution_ids, ["exec-new", "exec-long-running"]);
            assert!(!page.lagged);
            assert!(!page.has_more);
            assert!(reader
                .read_tail(&tenant, "app", Some(&page.cursor))
                .await
                .unwrap()
                .execution_ids
                .is_empty());
            assert!(reader
                .read_tail(&tenant, "app", None)
                .await
                .unwrap()
                .execution_ids
                .is_empty());
            assert!(reader
                .read_tail(&tenant, "other-app", None)
                .await
                .unwrap()
                .execution_ids
                .is_empty());
            assert!(reader
                .read_tail("other-tenant", &tenant, None)
                .await
                .unwrap()
                .execution_ids
                .is_empty());
            for i in 0..CAPACITY + 1 {
                publisher
                    .publish_tail(&tenant, "app", &format!("exec-{i}"))
                    .await
                    .unwrap();
            }
            assert_eq!(
                redis::cmd("XLEN")
                    .arg(key(&tenant, "app"))
                    .query_async::<usize>(&mut conn)
                    .await
                    .unwrap(),
                CAPACITY
            );
            let mut page = reader
                .read_tail(&tenant, "app", Some(&page.cursor))
                .await
                .unwrap();
            assert!(page.lagged);
            let mut ids = page.execution_ids.clone();
            while page.has_more {
                page = reader
                    .read_tail(&tenant, "app", Some(&page.cursor))
                    .await
                    .unwrap();
                assert!(!page.lagged);
                ids.extend(page.execution_ids.clone());
            }
            assert_eq!(
                ids,
                (1..=CAPACITY)
                    .map(|i| format!("exec-{i}"))
                    .collect::<Vec<_>>()
            );
            let _: bool = conn.expire(key(&tenant, "app"), 12).await.unwrap();
            publisher
                .publish_tail(&tenant, "app", "last")
                .await
                .unwrap();
            assert!(conn.ttl::<_, i64>(key(&tenant, "app")).await.unwrap() <= 12);
            let _: usize = conn.del(key(&tenant, "app")).await.unwrap();
            let reset = reader
                .read_tail(&tenant, "app", Some(&page.cursor))
                .await
                .unwrap();
            assert!(reset.lagged);
            assert!(reset.execution_ids.is_empty());
            assert!(conn.ttl::<_, i64>(key(&tenant, "app")).await.unwrap() > 12);
            publisher
                .publish_tail(&tenant, "app", "after-reset")
                .await
                .unwrap();
            assert_eq!(
                reader
                    .read_tail(&tenant, "app", Some(&reset.cursor))
                    .await
                    .unwrap()
                    .execution_ids,
                ["after-reset"]
            );
        }
    }
}
