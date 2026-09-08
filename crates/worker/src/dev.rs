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
use hibana_shared::ResourceLimits;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};
use tokio::sync::Semaphore;
use wasmtime::component::Component;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    vars: BTreeMap<String, String>,
    resources: ResourceLimits,
}

struct DevState {
    runtime: Runtime,
    component: Arc<crate::runtime::PreparedComponent>,
    settings: Settings,
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
    let engine = crate::runtime::build_engine()?;
    let component =
        Component::from_file(&engine, component.context("--dev-component is required")?)?;
    let state = Arc::new(DevState {
        runtime: Runtime::new(engine, metrics::Metrics::init())?,
        component: Arc::new(crate::runtime::PreparedComponent::new(component)?),
        settings,
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
    let (stream, response) = crate::runtime::response_channel();
    let body_tx = stream.body.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let env = env::build_env(
            &state.settings.vars,
            &BTreeMap::new(),
            &state.settings.vars.keys().cloned().collect(),
        );
        let result = state
            .runtime
            .run_http(
                crate::runtime::Invocation {
                    component: state.component.clone(),
                    request,
                    limits: state.settings.resources,
                    built_env: env,
                    allowed_addrs: Default::default(),
                },
                stream,
            )
            .await;
        if result.is_err() {
            let _ = body_tx
                .send(Err(std::io::Error::other("Wasm invocation failed")))
                .await;
        }
        // Keep EOF behind handler completion, including waitUntil and resource checks.
    });
    match response.receive().await {
        Ok((parts, body)) => Response::from_parts(parts, Body::from_stream(body)),
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Wasm handler did not return an HTTP response",
        )
            .into_response(),
    }
}
