//! Close public admission across replicas while retaining internal completion APIs.
use crate::{error::AppError, state::AppState};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
pub(crate) struct Requests(AtomicUsize);
struct RequestGuard(Arc<Requests>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Requests {
    fn enter(self: &Arc<Self>) -> RequestGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        RequestGuard(self.clone())
    }
    pub(crate) fn active(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub(crate) async fn enabled(pool: &sqlx::PgPool) -> Result<bool, sqlx::Error> {
    // No cache: a new/restarted replica must immediately honor maintenance.
    sqlx::query_scalar("SELECT owner IS NOT NULL FROM platform_maintenance WHERE singleton")
        .fetch_one(pool)
        .await
}

pub(crate) async fn public_gate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // Only actual management health routes are exempt (an app may use /healthz).
    let health = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .is_some_and(|p| matches!(p.as_str(), "/healthz" | "/readyz" | "/metrics"));
    if health {
        return next.run(req).await;
    }
    // Register before the DB read so pre-gate requests cannot escape the drain.
    let _guard = state.public_requests().enter();
    match enabled(state.pool()).await {
        Ok(false) => next.run(req).await,
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "platform admission unavailable",
        )
            .into_response(),
    }
    // Management writes finish before returning headers. Streaming guest requests
    // remain tracked by executions.pending/running until their durable completion.
}

pub(crate) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    crate::handlers::tenants::require_bootstrap_admin(&headers, &state)?;
    if !enabled(state.pool()).await? {
        return Ok((StatusCode::CONFLICT, "maintenance gate is open").into_response());
    }
    Ok(
        axum::Json(serde_json::json!({"active_requests": state.public_requests().active()}))
            .into_response(),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct PreparationRequest {
    pub owner: String,
    pub workers: std::collections::BTreeSet<std::net::IpAddr>,
}

pub(crate) async fn prepare(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<PreparationRequest>,
) -> Result<Response, AppError> {
    crate::handlers::tenants::require_bootstrap_admin(&headers, &state)?;
    let owner: Option<String> =
        sqlx::query_scalar("SELECT owner FROM platform_maintenance WHERE singleton")
            .fetch_one(state.pool())
            .await?;
    if owner.as_deref() != Some(request.owner.as_str()) {
        return Ok((StatusCode::CONFLICT, "maintenance owner mismatch").into_response());
    }
    if request.workers.is_empty() || request.workers.len() > 1024 {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(240),
        crate::preparation::prepare_active(&state, &request.workers),
    )
    .await
    {
        Ok(Ok(())) => Ok(StatusCode::NO_CONTENT.into_response()),
        _ => Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            "active application preparation incomplete",
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canceled_requests_release_drain_guard() {
        let requests = Arc::new(Requests::default());
        let first = requests.enter();
        let second = requests.enter();
        assert_eq!(requests.active(), 2);
        drop(first);
        assert_eq!(requests.active(), 1);
        drop(second);
        assert_eq!(requests.active(), 0);
    }
}
