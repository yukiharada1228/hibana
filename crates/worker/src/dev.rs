//! Standalone HTTP development server. Uses the fleet runtime without infrastructure credentials.
use crate::{env, metrics, runtime::Runtime};
use anyhow::{Context, Result};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    serve::ListenerExt as _,
    Router,
};
use hibana_shared::{egress::EgressEndpoint, ResourceLimits};
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio::time::{timeout_at, Instant};
use wasmtime::component::Component;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    vars: BTreeMap<String, String>,
    resources: ResourceLimits,
    #[serde(default)]
    net_allow_outbound: Vec<String>,
}

struct DevState {
    runtime: Runtime,
    component: Arc<crate::runtime::PreparedComponent>,
    settings: Settings,
    outbound: Vec<EgressEndpoint>,
    slots: Arc<Semaphore>,
}

pub async fn run(args: &[String]) -> Result<()> {
    let mut component = None;
    let mut settings = None;
    let mut bind: SocketAddr = "127.0.0.1:8787".parse()?;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = args.next().context("Expected an option value")?;
        match arg.as_str() {
            "--dev-component" => component = Some(value),
            "--dev-settings" => settings = Some(value),
            "--bind" => bind = value.parse()?,
            _ => anyhow::bail!("Unknown development option: {arg}"),
        }
    }
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "Development server must bind to a loopback address"
    );
    let settings: Settings = serde_json::from_slice(
        &tokio::fs::read(settings.context("--dev-settings is required")?).await?,
    )?;
    settings.resources.validate()?;
    let outbound = parse_outbound(&settings.net_allow_outbound)?;
    env::build_env(
        &settings.vars,
        &BTreeMap::new(),
        &settings.vars.keys().cloned().collect(),
    )
    .map_err(anyhow::Error::msg)?;
    let engine = crate::runtime::build_engine()?;
    let component =
        Component::from_file(&engine, component.context("--dev-component is required")?)?;
    let state = Arc::new(DevState {
        runtime: Runtime::new(engine, metrics::Metrics::init())?,
        component: Arc::new(crate::runtime::PreparedComponent::new(component)?),
        settings,
        outbound,
        slots: Arc::new(Semaphore::new(8)),
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Hibana (Wasmtime): http://{}", listener.local_addr()?);
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::warn!(%error, "cannot enable TCP_NODELAY for HTTP connection");
        }
    });
    axum::serve(listener, Router::new().fallback(invoke).with_state(state))
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
    Ok(())
}

async fn invoke(State(state): State<Arc<DevState>>, request: Request<Body>) -> Response {
    let Ok(slot) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, hibana_shared::http::MAX_REQUEST_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let request = hibana_shared::http::HttpRequest::from_parts(&parts, &body);
    let is_head = request.method == "HEAD";
    let (stream, response, completed) = crate::runtime::response_channel();
    tokio::spawn(async move {
        let _slot = slot;
        let env = match env::build_env(
            &state.settings.vars,
            &BTreeMap::new(),
            &state.settings.vars.keys().cloned().collect(),
        ) {
            Ok(env) => env,
            Err(message) => {
                tracing::warn!(reason = message, "invalid development environment");
                return;
            }
        };
        let deadline = Instant::now() + state.settings.resources.max_execution_time();
        let approved_egress = resolve_outbound(&state.runtime, &state.outbound, deadline).await;
        let result = state
            .runtime
            .run_http(
                crate::runtime::Invocation {
                    component: state.component.clone(),
                    request,
                    limits: state.settings.resources,
                    built_env: env,
                    approved_egress,
                    deadline,
                },
                stream,
            )
            .await;
        if result.is_ok() {
            let _ = completed.send(());
        }
        // Keep EOF behind handler completion, including waitUntil and resource checks.
    });
    match response.receive(is_head).await {
        Ok((parts, body)) => Response::from_parts(parts, Body::from_stream(body)),
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Wasm handler did not return an HTTP response",
        )
            .into_response(),
    }
}

fn parse_outbound(entries: &[String]) -> Result<Vec<EgressEndpoint>> {
    anyhow::ensure!(
        entries.len() <= 64,
        "At most 64 development destinations are allowed"
    );
    entries
        .iter()
        .map(|entry| {
            let ep = hibana_shared::egress::parse_egress_endpoint(entry).map_err(|_| {
                anyhow::anyhow!("Development destinations must be HOST:PORT or [IPv6]:PORT")
            })?;
            let hostname = ep.host.trim_end_matches('.');
            let valid = if entry.trim().starts_with('[') {
                ep.host.parse::<std::net::Ipv6Addr>().is_ok()
            } else {
                ep.host.parse::<std::net::Ipv4Addr>().is_ok()
                    || (hostname.len() <= 253
                        && hostname.split('.').all(|label| {
                            !label.is_empty()
                                && label.len() <= 63
                                && !label.starts_with('-')
                                && !label.ends_with('-')
                                && label
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        }))
            };
            anyhow::ensure!(
                valid,
                "Development destinations must not contain URLs, credentials or wildcards"
            );
            Ok(ep)
        })
        .collect()
}

// Resolve only the developer's explicit destinations. Each invocation pins its
// DNS snapshot and still applies the runtime's public-IP and exact-port policy.
async fn resolve_outbound(
    runtime: &Runtime,
    endpoints: &[EgressEndpoint],
    deadline: Instant,
) -> crate::runtime::ApprovedEgress {
    let mut approved = crate::runtime::ApprovedEgress::default();
    let dns_deadline = deadline.min(Instant::now() + Duration::from_secs(5));
    for ep in endpoints {
        match timeout_at(
            dns_deadline,
            tokio::net::lookup_host((ep.host.as_str(), ep.port)),
        )
        .await
        {
            Ok(Ok(addresses)) => {
                for addr in addresses {
                    if runtime.tcp_destination_allowed(addr) {
                        approved.insert(&ep.host, addr);
                    } else {
                        tracing::warn!(host = %ep.host, port = ep.port, "development destination resolved to a denied IP; skipping");
                    }
                }
            }
            Ok(Err(_)) => {
                tracing::warn!(host = %ep.host, port = ep.port, "could not resolve development destination; skipping")
            }
            Err(_) => {
                tracing::warn!("development destination DNS deadline exceeded");
                break;
            }
        }
    }
    approved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_settings_default_to_no_destinations_and_reject_invalid_entries() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "vars": {}, "resources": ResourceLimits::default(),
        }))
        .unwrap();
        assert!(parse_outbound(&settings.net_allow_outbound)
            .unwrap()
            .is_empty());
        for entry in [
            "",
            "db",
            "db:0",
            "db:65536",
            "https://db:443",
            "user:sensitive-value@db:5432",
            "*.example:443",
            "[::g]:443",
            "[host]:443",
        ] {
            let error = parse_outbound(&[entry.into()]).unwrap_err().to_string();
            assert!(!error.contains("sensitive-value"));
        }
        assert!(parse_outbound(&vec!["db:5432".into(); 65]).is_err());
        assert_eq!(
            parse_outbound(&["db.example.com:5432".into()]).unwrap()[0].port,
            5432
        );
    }

    #[tokio::test]
    async fn development_dns_snapshot_keeps_exact_destinations_and_blocks_internal_ips() {
        let runtime = Runtime::new(
            crate::runtime::build_engine().unwrap(),
            metrics::Metrics::init(),
        )
        .unwrap();
        let entries = [
            "1.1.1.1:5432",
            "127.0.0.1:5432",
            "10.0.0.1:5432",
            "169.254.169.254:80",
            "[::1]:5432",
            "[::ffff:127.0.0.1]:5432",
        ]
        .map(str::to_owned);
        let endpoints = parse_outbound(&entries).unwrap();
        let approved = resolve_outbound(
            &runtime,
            &endpoints,
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            approved.resolve("1.1.1.1").collect::<Vec<_>>(),
            ["1.1.1.1:5432".parse::<SocketAddr>().unwrap()]
        );
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "::ffff:127.0.0.1",
            "1.0.0.1",
            "unapproved.example",
        ] {
            assert!(approved.resolve(host).next().is_none(), "{host}");
        }
        let empty = resolve_outbound(&runtime, &[], Instant::now() + Duration::from_secs(5)).await;
        assert!(empty.resolve("1.1.1.1").next().is_none());
    }
}
