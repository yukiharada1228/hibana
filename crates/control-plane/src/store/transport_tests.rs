//! Real Redis transport tests; scripts/test-redis-tls.sh supplies disposable servers.
use super::{RateLimitParams, RedisStore, Store};

fn endpoint(name: &str) -> String {
    let value = std::env::var(name).expect("run scripts/test-redis-tls.sh");
    let url = reqwest::Url::parse(&value).unwrap();
    assert!(matches!(url.host_str(), Some("localhost" | "127.0.0.1")));
    value
}

#[test]
fn redis_transport_accepts_tls_urls() {
    let client = super::redis_client("rediss://localhost:6379/0").unwrap();
    assert!(matches!(
        client.get_connection_info().addr,
        redis::ConnectionAddr::TcpTls {
            insecure: false,
            ..
        }
    ));
}

#[tokio::test]
#[ignore = "requires scripts/test-redis-tls.sh"]
async fn redis_transport_runs_store_operations_over_tcp_and_tls() {
    for name in ["HIBANA_TEST_REDIS_TCP_URL", "HIBANA_TEST_REDIS_TLS_URL"] {
        let store = RedisStore::connect(&endpoint(name)).await.unwrap();
        store.ping().await.unwrap();
        let key = hibana_shared::new_execution_id();
        let rate = RateLimitParams {
            refill_per_sec: 1.0,
            capacity: 1.0,
        };
        assert!(store.rate_limit(&key, rate).await.unwrap().allowed);
        assert!(!store.rate_limit(&key, rate).await.unwrap().allowed);
    }
}

#[tokio::test]
#[ignore = "requires scripts/test-redis-tls.sh"]
async fn redis_transport_rate_limits_share_server_time_and_atomic_budget() {
    use redis::AsyncCommands as _;

    for name in ["HIBANA_TEST_REDIS_TCP_URL", "HIBANA_TEST_REDIS_TLS_URL"] {
        let url = endpoint(name);
        let stores = [
            RedisStore::connect(&url).await.unwrap(),
            RedisStore::connect(&url).await.unwrap(),
        ];
        let mut conn = super::redis_client(&url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        let tenant = hibana_shared::new_execution_id();
        let key = format!("rl:{tenant}");
        // Slow refill keeps these assertions independent of scheduling jitter.
        let rate = RateLimitParams {
            refill_per_sec: 0.001,
            capacity: 2.0,
        };
        let before = server_millis(&mut conn).await;
        let results =
            futures::future::join_all((0..20).map(|i| stores[i % 2].rate_limit(&tenant, rate)))
                .await;
        assert_eq!(
            results
                .into_iter()
                .filter(|r| r.as_ref().unwrap().allowed)
                .count(),
            2,
        );
        let last: u64 = conn.hget(&key, "last").await.unwrap();
        assert!((before..=server_millis(&mut conn).await).contains(&last));
        let ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!((1..=2060).contains(&ttl));
        assert!(
            stores[0]
                .rate_limit(&hibana_shared::new_execution_id(), rate)
                .await
                .unwrap()
                .allowed
        );

        // Simulate a server clock rollback: an empty bucket must retain its
        // future refill watermark across requests from both Control Planes.
        let future = server_millis(&mut conn).await + 60_000;
        let _: () = conn.hset(&key, "last", future).await.unwrap();
        for store in &stores {
            let result = store.rate_limit(&tenant, rate).await.unwrap();
            assert!(!result.allowed);
            assert!(result.retry_after_secs > 0);
            assert_eq!(conn.hget::<_, _, u64>(&key, "last").await.unwrap(), future);
        }

        // An idle bucket refills only to capacity, even after a long interval.
        let past = server_millis(&mut conn).await - 3_000_000;
        let _: () = conn.hset(&key, "last", past).await.unwrap();
        assert!(stores[0].rate_limit(&tenant, rate).await.unwrap().allowed);
        assert!(stores[1].rate_limit(&tenant, rate).await.unwrap().allowed);
        assert!(!stores[0].rate_limit(&tenant, rate).await.unwrap().allowed);
    }
}

async fn server_millis(conn: &mut redis::aio::MultiplexedConnection) -> u64 {
    let (seconds, micros): (u64, u64) = redis::cmd("TIME").query_async(conn).await.unwrap();
    seconds * 1000 + micros / 1000
}

#[tokio::test]
#[ignore = "requires scripts/test-redis-tls.sh"]
async fn redis_transport_rejects_untrusted_certificates_and_wrong_hostnames() {
    let trusted = endpoint("HIBANA_TEST_REDIS_TLS_URL");
    let mut wrong_host = reqwest::Url::parse(&trusted).unwrap();
    wrong_host.set_host(Some("127.0.0.1")).unwrap();
    let untrusted = endpoint("HIBANA_TEST_REDIS_UNTRUSTED_URL");
    for url in [
        untrusted,
        wrong_host.to_string(),
        format!("{trusted}#insecure"),
    ] {
        assert!(
            RedisStore::connect(&url).await.is_err(),
            "TLS verification must be enforced"
        );
    }
}
