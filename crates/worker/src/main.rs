//! Worker binary composition. Both dev and fleet execution use the same runtime.
mod artifacts;
mod cache_storage;
mod capacity;
mod compiler;
mod config;
mod control_plane;
mod dev;
mod direct_http;
mod env;
mod lifecycle;
mod metrics;
mod repository;
mod runtime;
mod service;
use config::Settings;
use lifecycle::{spawn_metrics_server, spawn_shutdown_listener, Shutdown};
use service::Worker;
use std::{sync::Arc, time::Duration};
use tracing::{info, warn};
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-V") {
        println!("hibana-worker {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == compiler::FLAG) {
        return compiler::run(&args);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: Vec<String>) -> anyhow::Result<()> {
    let log_format = std::env::var("LOG_FORMAT")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "text".into());
    let _otel_guard = hibana_shared::otel::init_tracing(&log_format, "info", "hibana-worker");
    if !args.is_empty() {
        return dev::run(&args).await;
    }
    let settings = Settings::from_env()?;
    let metrics = metrics::Metrics::init();
    let worker = Arc::new(Worker::connect(&settings, metrics.clone()).await?);
    let shutdown = Shutdown::new();
    spawn_shutdown_listener(shutdown.clone(), settings.drain_timeout_secs);
    spawn_metrics_server(metrics, settings.metrics_bind_addr, shutdown.clone());
    let server = direct_http::start(worker, shutdown.clone(), &settings.http_bind_addr).await?;
    info!("HTTP worker started");
    tokio::select! {
        result = server => result??,
        _ = async {
            shutdown.wait().await;
            tokio::time::sleep(Duration::from_secs(settings.drain_timeout_secs)).await;
        } => warn!("HTTP drain deadline reached; stopping worker"),
    }
    Ok(())
}
