//! Real Redis transport tests; scripts/test-redis-tls.sh supplies disposable servers.
use super::{InflightParams, RateLimitParams, RedisStore, Store};

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
        assert!(store.rate_limit(&key, rate, 1000).await.unwrap().allowed);
        assert!(!store.rate_limit(&key, rate, 1000).await.unwrap().allowed);
        let limit = InflightParams {
            max: 1,
            ttl_secs: 60,
        };
        assert!(store.reserve_inflight(&key, limit).await.unwrap().admitted);
        assert!(!store.reserve_inflight(&key, limit).await.unwrap().admitted);
        assert_eq!(store.release_inflight(&key).await.unwrap(), 0);
    }
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
