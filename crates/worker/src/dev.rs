//! Standalone HTTP development server. Uses the fleet runtime without infrastructure credentials.
use crate::{env, metrics, runtime::Runtime};
use anyhow::{Context, Result};
use axum::{
    body::{to_bytes, Body, Bytes, HttpBody},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    serve::ListenerExt as _,
    Router,
};
use hibana_shared::{egress::EgressEndpoint, ResourceLimits};
use serde::Deserialize;
use std::{collections::BTreeMap, future::Future, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio::time::{timeout, timeout_at, Instant};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use wasmtime::component::Component;

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

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
    shutdown: CancellationToken,
    executions: TaskTracker,
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
        shutdown: CancellationToken::new(),
        executions: TaskTracker::new(),
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Hibana (Wasmtime): http://{}", listener.local_addr()?);
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::warn!(%error, "cannot enable TCP_NODELAY for HTTP connection");
        }
    });
    let shutdown = state.shutdown.clone();
    let signal_shutdown = shutdown.clone();
    let executions = state.executions.clone();
    // Let admitted handlers finish, with one second to flush the response. The
    // validated execution limit is at most 60s, below the CLI's 65s kill timer.
    let drain_timeout = state.settings.resources.max_execution_time() + Duration::from_secs(1);
    let server = async move {
        axum::serve(listener, Router::new().fallback(invoke).with_state(state))
            .with_graceful_shutdown(async move {
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
                signal_shutdown.cancel();
            })
            .await?;
        // HEAD/204/304 may finish sending before the guest's waitUntil work.
        executions.close();
        executions.wait().await;
        Ok(())
    };
    drain_on_shutdown(server, shutdown, drain_timeout).await?;
    Ok(())
}

async fn drain_on_shutdown(
    server: impl Future<Output = std::io::Result<()>>,
    shutdown: CancellationToken,
    drain_timeout: Duration,
) -> std::io::Result<()> {
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        _ = shutdown.cancelled() => {
            match timeout(drain_timeout, server).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!("development server drain deadline exceeded");
                    Ok(())
                }
            }
        }
    }
}

async fn invoke(State(state): State<Arc<DevState>>, request: Request<Body>) -> Response {
    if state.shutdown.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(slot) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (parts, body) = request.into_parts();
    let body = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        result = read_body(body) => match result {
            Ok(body) => body,
            Err(status) => return status.into_response(),
        },
    };
    let request = hibana_shared::http::HttpRequest::from_parts(&parts, &body);
    let is_head = request.method == "HEAD";
    let (stream, response, completed) = crate::runtime::response_channel();
    state.executions.clone().spawn(async move {
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

async fn read_body(body: Body) -> std::result::Result<Bytes, StatusCode> {
    let limit = hibana_shared::http::MAX_REQUEST_BYTES;
    if body.size_hint().lower() > limit as u64 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    match timeout(RECEIVE_TIMEOUT, to_bytes(body, limit)).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => {
            use std::error::Error as _;
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                Err(StatusCode::PAYLOAD_TOO_LARGE)
            } else {
                Err(StatusCode::BAD_REQUEST)
            }
        }
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT),
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

    fn state() -> Arc<DevState> {
        let engine = crate::runtime::build_engine().unwrap();
        let component = Component::new(&engine, "(component)").unwrap();
        Arc::new(DevState {
            runtime: Runtime::new(engine, metrics::Metrics::init()).unwrap(),
            component: Arc::new(crate::runtime::PreparedComponent::new(component).unwrap()),
            settings: Settings {
                vars: BTreeMap::new(),
                resources: ResourceLimits::default(),
                net_allow_outbound: Vec::new(),
            },
            outbound: Vec::new(),
            slots: Arc::new(Semaphore::new(8)),
            shutdown: CancellationToken::new(),
            executions: TaskTracker::new(),
        })
    }

    fn pending_request() -> Request<Body> {
        Request::new(Body::from_stream(futures::stream::pending::<
            Result<Bytes, std::io::Error>,
        >()))
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_requests_timeout_and_release_all_slots() {
        let state = state();
        let requests = futures::future::join_all(
            (0..8).map(|_| invoke(State(state.clone()), pending_request())),
        );
        tokio::pin!(requests);
        assert!(futures::poll!(&mut requests).is_pending());
        assert_eq!(state.slots.available_permits(), 0);
        assert_eq!(
            invoke(State(state.clone()), Request::new(Body::empty()))
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        tokio::time::advance(RECEIVE_TIMEOUT).await;
        for response in requests.await {
            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        }
        assert_eq!(state.slots.available_permits(), 8);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_incomplete_requests_and_rejects_new_ones() {
        let state = state();
        let request = invoke(State(state.clone()), pending_request());
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        let started = Instant::now();
        state.shutdown.cancel();
        assert_eq!(request.await.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state.slots.available_permits(), 8);
        assert_eq!(
            invoke(State(state), Request::new(Body::empty()))
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn body_deadline_is_absolute_and_size_errors_are_distinct() {
        let trickle = Body::from_stream(futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from_static(b"x")), ()))
        }));
        let started = Instant::now();
        assert_eq!(read_body(trickle).await, Err(StatusCode::REQUEST_TIMEOUT));
        assert_eq!(started.elapsed(), RECEIVE_TIMEOUT);
        let limit = hibana_shared::http::MAX_REQUEST_BYTES;
        assert_eq!(
            read_body(Body::from(vec![0; limit + 1])).await,
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        );
        let streamed = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![0; limit])),
            Ok(Bytes::from_static(b"x")),
        ]));
        assert_eq!(
            read_body(streamed).await,
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        );
        let broken = Body::from_stream(futures::stream::iter([Err::<Bytes, _>(
            std::io::Error::other("fixture"),
        )]));
        assert_eq!(read_body(broken).await, Err(StatusCode::BAD_REQUEST));
        assert_eq!(read_body(Body::from("ok")).await.unwrap(), "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn drain_deadline_starts_at_shutdown_and_bounds_stalled_connections() {
        let shutdown = CancellationToken::new();
        let signal = shutdown.clone();
        let server = async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            signal.cancel();
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        };
        let started = Instant::now();
        drain_on_shutdown(server, shutdown, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(11));

        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let started = Instant::now();
        drain_on_shutdown(futures::future::pending(), shutdown, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

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
