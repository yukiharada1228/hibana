//! Close public admission across replicas while retaining internal completion APIs.
use crate::{error::AppError, state::AppState};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hibana_database::prelude::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
pub(crate) struct Requests {
    active: AtomicUsize,
    // A status writer fences in-progress gate reads. Only accepted requests are
    // counted, and no pre-close read can register after status observes idle.
    checking: tokio::sync::RwLock<()>,
}
struct RequestGuard(Arc<Requests>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Requests {
    fn enter(self: &Arc<Self>) -> RequestGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        RequestGuard(self.clone())
    }
    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

pub(crate) async fn enabled(pool: &sea_orm::DatabaseConnection) -> Result<bool, sea_orm::DbErr> {
    // No cache: a new/restarted replica must immediately honor maintenance.
    Ok(platform_maintenance::Entity::find_by_id(true)
        .one(pool)
        .await?
        .ok_or_else(|| DbErr::Custom("maintenance state missing".into()))?
        .owner
        .is_some())
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
    let checking = state.public_requests().checking.read().await;
    match enabled(state.pool()).await {
        Ok(false) => {
            let _guard = state.public_requests().enter();
            drop(checking);
            next.run(req).await
        }
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
    // Tokio's fair writer blocks new gate checks while earlier readers finish.
    // Rejected traffic cannot keep the drain busy, even under sustained load.
    let checking = state.public_requests().checking.write().await;
    if !enabled(state.pool()).await? {
        return Ok((StatusCode::CONFLICT, "maintenance gate is open").into_response());
    }
    // A handler may finish (and leave a streaming execution running) during the
    // DB reads. Remember the earlier count so that transition cannot look idle.
    let active_before = state.public_requests().active();
    drop(checking);
    let mut executions = 0i64;
    for (tenant, _) in crate::db::list_tenants_for_admin(state.pool()).await? {
        let tx = state.pool().begin().await?;
        crate::db::set_tenant_guc(&tx, &tenant).await?;
        executions += crate::db::count_inflight_executions(&tx, &tenant).await?;
        tx.commit().await?;
    }
    Ok(
        axum::Json(serde_json::json!({"active_requests": active_before.max(state.public_requests().active()), "inflight_executions": executions}))
            .into_response(),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct GateRequest {
    owner: String,
    closed: bool,
}

pub(crate) async fn set(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<GateRequest>,
) -> Result<Response, AppError> {
    crate::handlers::tenants::require_bootstrap_admin(&headers, &state)?;
    if request.owner.is_empty()
        || request.owner.len() > 128
        || !request
            .owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    let changed: bool = state
        .pool()
        .query_one(
            &Query::select()
                .expr(
                    Func::cust("hibana_set_maintenance")
                        .args([Expr::val(request.owner), Expr::val(request.closed)]),
                )
                .to_owned(),
        )
        .await?
        .ok_or_else(|| DbErr::Custom("maintenance result missing".into()))?
        .try_get_by_index(0)?;
    Ok(if changed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CONFLICT
    }
    .into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct PreparationRequest {
    pub owner: Option<String>,
    pub workers: std::collections::BTreeSet<std::net::IpAddr>,
}

pub(crate) async fn prepare(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<PreparationRequest>,
) -> Result<Response, AppError> {
    crate::handlers::tenants::require_bootstrap_admin(&headers, &state)?;
    let owner: Option<String> = platform_maintenance::Entity::find_by_id(true)
        .one(state.pool())
        .await?
        .ok_or_else(|| DbErr::Custom("maintenance state missing".into()))?
        .owner;
    // An open platform can be checked without closing admission. During a stop
    // or recovery only the matching owner may prepare the fleet for reopening.
    if owner != request.owner {
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

    #[tokio::test]
    async fn drain_waits_for_preclose_admission_and_fences_new_checks() {
        let requests = Arc::new(Requests::default());
        let preclose_read = requests.checking.read().await;
        let status = requests.checking.write();
        tokio::pin!(status);
        assert!(futures::poll!(&mut status).is_pending());
        // The queued writer cannot be starved by requests arriving after closure.
        assert!(requests.checking.try_read().is_err());
        let accepted = requests.enter();
        drop(preclose_read);
        let fence = status.await;
        assert_eq!(
            requests.active(),
            1,
            "a delayed pre-close read remains visible"
        );
        drop(fence);
        drop(accepted);
        let _rejected_read = requests.checking.read().await;
        assert_eq!(
            requests.active(),
            0,
            "rejected requests need no drain guard"
        );
    }
}
