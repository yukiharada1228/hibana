//! Internal HTTP transport: redeem a one-use job token, admit concurrency, stream a response.
use crate::{
    lifecycle::Shutdown,
    service::{InflightGuard, Worker},
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
struct HttpState {
    worker: Arc<Worker>,
    shutdown: Arc<Shutdown>,
}

async fn invoke(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if state.shutdown.is_draining() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(slot) = state.worker.execution_slots.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Worker concurrency limit reached",
        )
            .into_response();
    };
    let Some(token) = headers.get("x-hibana-job-token") else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // Redeem against a fixed CP endpoint. No URL, source, tenant or resource limit
    // supplied in the HTTP body is trusted. Invalid/replayed tokens never run a guest.
    let job = match state.worker.control_plane.redeem(token).await {
        Ok(job) => job,
        Err(status) => return status.into_response(),
    };
    let (stream, response) = crate::runtime::response_channel();
    let body_tx = stream.body.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let _inflight = InflightGuard::new(&state.worker.metrics);
        if !state.worker.handle_http(job, stream).await {
            let _ = body_tx
                .send(Err(std::io::Error::other(
                    "Invocation result could not be persisted",
                )))
                .await;
        }
        // EOF only after execution and durable result publication.
    });
    match response.receive().await {
        Ok((parts, body)) => Response::from_parts(parts, Body::from_stream(body)),
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Worker did not produce an HTTP response",
        )
            .into_response(),
    }
}

pub async fn start(
    worker: Arc<Worker>,
    shutdown: Arc<Shutdown>,
    bind_addr: &str,
) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let state = HttpState {
        worker,
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/invoke", post(invoke))
        .layer(axum::extract::DefaultBodyLimit::max(0))
        .with_state(state);
    Ok(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.wait().await;
            })
            .await?;
        Ok(())
    }))
}
