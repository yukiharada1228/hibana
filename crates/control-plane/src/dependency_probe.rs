//! Read-only installation probes. Each invocation opens fresh connections in the
//! target Pod's network namespace; credentials and transport errors stay there.
use sqlx::Connection;
use std::{future::Future, net::IpAddr, time::Duration};

async fn checked(check: impl Future<Output = anyhow::Result<()>>) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(8), check).await,
        Ok(Ok(()))
    )
}

async fn database() -> anyhow::Result<()> {
    let mut connection = sqlx::PgConnection::connect(&std::env::var("DATABASE_URL")?).await?;
    sqlx::query("SELECT 1").execute(&mut connection).await?;
    connection.close().await?;
    Ok(())
}

async fn redis() -> anyhow::Result<()> {
    let client = redis::Client::open(std::env::var("REDIS_URL")?)?;
    let mut connection = client.get_multiplexed_async_connection().await?;
    let pong: String = redis::cmd("PING").query_async(&mut connection).await?;
    anyhow::ensure!(pong == "PONG", "invalid Redis response");
    Ok(())
}

async fn object_store() -> anyhow::Result<()> {
    let mut url = reqwest::Url::parse(&std::env::var("S3_ENDPOINT")?)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "invalid S3 scheme"
    );
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid S3 endpoint"))?
        .pop_if_empty()
        .push(&std::env::var("S3_BUCKET")?);
    let response = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .build()?
        .head(url)
        .send()
        .await?;
    // Workers have no S3 credentials. A private bucket denies an unsigned HEAD;
    // verify transport/TLS without requiring additional bucket-list permissions.
    let status = response.status();
    anyhow::ensure!(
        status.is_success() || matches!(status.as_u16(), 403 | 404),
        "S3 endpoint unavailable"
    );
    Ok(())
}

async fn peers(url: &str, addresses: &[IpAddr]) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing peer host"))?;
    // URL serialization brackets IPv6 literals; the TCP resolver needs the IP.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("missing peer port"))?;
    // Cover the configured Service/DNS path and every current peer's ingress.
    tokio::net::TcpStream::connect((host, port)).await?;
    for &address in addresses {
        tokio::net::TcpStream::connect(std::net::SocketAddr::new(address, port)).await?;
    }
    Ok(())
}

pub(crate) async fn run(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.len() == 2 && matches!(args[0].as_str(), "control-plane" | "worker"),
        "Usage: --maintenance check control-plane|worker PEER_IPS"
    );
    let addresses: Vec<IpAddr> = args[1]
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("Invalid probe peer IPs"))?;
    let cp = args[0] == "control-plane";
    let connection = async {
        let name = if cp {
            "WORKER_HTTP_URL"
        } else {
            "CONTROL_PLANE_INTERNAL_URL"
        };
        peers(&std::env::var(name)?, &addresses).await
    };
    let (database, object_store, peers, redis) = tokio::join!(
        checked(database()),
        checked(object_store()),
        checked(connection),
        checked(async {
            if cp {
                redis().await
            } else {
                Ok(())
            }
        })
    );
    let mut checks = serde_json::json!({"database": database, "object_store": object_store});
    checks[if cp { "workers" } else { "control_planes" }] = peers.into();
    if cp {
        checks["redis"] = redis.into();
    }
    println!("{}", serde_json::json!({"protocol": 1, "checks": checks}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checks_service_and_each_peer_without_sending_credentials() {
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = service.local_addr().unwrap();
        let url = format!("http://{address}");
        peers(&url, &[address.ip()]).await.unwrap();
        // 127.0.0.2 is a different loopback peer, with no listener at this port.
        assert!(!checked(peers(&url, &["127.0.0.2".parse().unwrap()])).await);
        drop(service);
        assert!(peers(&url, &[address.ip()]).await.is_err());
    }

    #[tokio::test]
    async fn accepts_ipv6_literal_service_urls() {
        let service = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = service.local_addr().unwrap();
        assert!(checked(peers(&format!("http://{address}"), &[address.ip()])).await);
    }

    #[tokio::test]
    async fn rejects_unknown_actions_and_invalid_peers_before_loading_credentials() {
        for args in [
            vec![],
            vec!["invalid", "127.0.0.1"],
            vec!["worker", "private-credential"],
        ] {
            let error = run(&args.into_iter().map(String::from).collect::<Vec<_>>())
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("private-credential"));
        }
    }
}
