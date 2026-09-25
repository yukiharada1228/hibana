//! Graceful shutdown and process probes.
use crate::metrics;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// SIGTERM / Ctrl-C を待ってドレインを開始するタスクを起こす（§4.6）。
///
/// `drain_timeout_secs == 0` のときは**ハンドラを一切インストールしない**。既定の
/// シグナルによる即時終了を使用する。既定では処理中のHTTPを待つ。
pub(crate) fn spawn_shutdown_listener(shutdown: CancellationToken, drain_timeout_secs: u64) {
    if drain_timeout_secs == 0 {
        info!(
            "graceful drain disabled (WORKER_DRAIN_TIMEOUT_SECS=0); SIGTERM terminates immediately"
        );
        return;
    }
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    // ハンドラを張れないなら黙って「ドレインしない」に落ちる。ここで落ちる方が
                    // 「ドレインしているつもりで実は即死」より危険なので、loud に警告する。
                    warn!(error = %e, "could not install SIGTERM handler; drain will not run");
                    return;
                }
            };
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM; starting drain"),
            r = tokio::signal::ctrl_c() => match r {
                Ok(()) => info!("received Ctrl-C; starting drain"),
                Err(e) => { warn!(error = %e, "ctrl_c listener failed"); return; }
            },
        }
        shutdown.cancel();
    });
}

pub(crate) fn spawn_metrics_server(
    worker: Arc<crate::service::Worker>,
    bind_addr: String,
    shutdown: CancellationToken,
) {
    let metrics = worker.metrics.clone();
    tokio::spawn(async move {
        use axum::{extract::State as AxState, routing::get, Router};

        #[derive(Clone)]
        struct ProbeState {
            metrics: Arc<metrics::Metrics>,
            shutdown: CancellationToken,
        }

        async fn healthz() -> axum::http::StatusCode {
            axum::http::StatusCode::OK
        }
        async fn readyz(AxState(s): AxState<ProbeState>) -> axum::http::StatusCode {
            if s.shutdown.is_cancelled() {
                return axum::http::StatusCode::SERVICE_UNAVAILABLE;
            }
            // Cache residency and overload do not remove the entire
            // fleet from discovery. Draining still withdraws this Worker.
            axum::http::StatusCode::OK
        }
        async fn metrics_handler(
            AxState(s): AxState<ProbeState>,
        ) -> impl axum::response::IntoResponse {
            let (headers, body) = s.metrics.render();
            (axum::http::StatusCode::OK, headers, body)
        }

        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/readyz", get(readyz))
            .route("/metrics", get(metrics_handler))
            .with_state(ProbeState { metrics, shutdown });

        let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(addr = %bind_addr, error = %e, "worker metrics server: failed to bind; metrics unavailable");
                return;
            }
        };
        info!(addr = %bind_addr, "worker metrics server listening");
        if let Err(e) = axum::serve(listener, app).await {
            warn!(error = %e, "worker metrics server: serve exited with error");
        }
    });
}
