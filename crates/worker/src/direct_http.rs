//! Internal HTTP transport: reserve capacity, redeem a token, claim once, stream a response.
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
    serve::ListenerExt as _,
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
struct HttpState {
    worker: Arc<Worker>,
    shutdown: Arc<Shutdown>,
}

async fn invoke(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    let started = std::time::Instant::now();
    if state.shutdown.is_draining() {
        return reject_capacity(&state.worker, "draining");
    }
    let Ok(slot) = state.worker.execution_slots.clone().try_acquire_owned() else {
        return reject_capacity(&state.worker, "concurrency");
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
    let redeemed = std::time::Instant::now();
    let resolved = match state.worker.resolve_job(&job).await {
        Ok(resolved) => resolved,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let resolved_at = std::time::Instant::now();
    // Keep the Arc through execution. Eviction after this check cannot trigger a
    // compile or lose the prepared code. A cold Worker never claims the execution.
    let component = match state
        .worker
        .artifacts
        .cached_component(&job.wasm_sha256, true)
    {
        Ok(Some(component)) => component,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (hibana_shared::http::WORKER_REJECTED_HEADER, "not_prepared"),
                    ("retry-after", "1"),
                ],
                "Application is preparing on this Worker",
            )
                .into_response()
        }
    };
    let Some(memory) = state
        .worker
        .memory_budget
        .try_reserve(resolved.limits.max_memory_bytes)
    else {
        return reject_capacity(&state.worker, "memory");
    };
    tracing::debug!(target: "hibana_latency", execution_id = %job.execution_id,
        stage = "worker_admission", redeem_us = redeemed.duration_since(started).as_micros() as u64,
        resolve_us = resolved_at.duration_since(redeemed).as_micros() as u64,
        cache_us = resolved_at.elapsed().as_micros() as u64, "HTTP phase timing");
    let (stream, response) = crate::runtime::response_channel();
    let body_tx = stream.body.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let _memory = memory;
        let _inflight = InflightGuard::new(&state.worker.metrics);
        if !state
            .worker
            .handle_http(job, resolved, component, stream)
            .await
        {
            let _ = body_tx
                .send(Err(std::io::Error::other(
                    "Invocation result could not be persisted",
                )))
                .await;
        }
        // EOF only after execution and durable result publication.
    });
    match response.receive().await {
        Ok((mut parts, body)) => {
            // A guest 503 must never be mistaken for a safe-to-retry refusal.
            parts
                .headers
                .remove(hibana_shared::http::WORKER_REJECTED_HEADER);
            Response::from_parts(parts, Body::from_stream(body))
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Worker did not produce an HTTP response",
        )
            .into_response(),
    }
}

async fn prepare(
    State(state): State<HttpState>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    if state.shutdown.is_draining() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(_slot) = state.worker.preparation_slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(token) = headers.get(hibana_shared::preparation::TOKEN_HEADER) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let artifact = match state.worker.control_plane.redeem_artifact(token).await {
        Ok(artifact) => artifact,
        Err(status) => return status.into_response(),
    };
    if method == axum::http::Method::HEAD {
        return match state
            .worker
            .artifacts
            .cached_component(&artifact.sha256, false)
        {
            Ok(Some(_)) => StatusCode::NO_CONTENT,
            Ok(None) => StatusCode::SERVICE_UNAVAILABLE,
            Err(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
        .into_response();
    }
    let started = std::time::Instant::now();
    match state
        .worker
        .artifacts
        .prepare(&artifact.sha256, &artifact.url)
        .await
    {
        Ok(_) => {
            tracing::debug!(sha256 = %artifact.sha256, elapsed_ms = started.elapsed().as_millis() as u64, "artifact prepared");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => {
            tracing::warn!(sha256 = %artifact.sha256, "artifact preparation failed");
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
    }
}

fn reject_capacity(worker: &Worker, reason: &str) -> Response {
    worker
        .metrics
        .capacity_rejections_total
        .with_label_values(&[reason])
        .inc();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (hibana_shared::http::WORKER_REJECTED_HEADER, "capacity"),
            ("retry-after", "1"),
        ],
        "Worker capacity unavailable",
    )
        .into_response()
}

pub async fn start(
    worker: Arc<Worker>,
    shutdown: Arc<Shutdown>,
    bind_addr: &str,
) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::warn!(%error, "cannot enable TCP_NODELAY for HTTP connection");
        }
    });
    let state = HttpState {
        worker,
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/invoke", post(invoke))
        .route("/prepare", post(prepare).head(prepare))
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
