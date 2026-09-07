//! Internal invocation token authentication.
use crate::{error::AppError, state::AppState};
use axum::http::HeaderMap;
use faas_shared::FaasError;
const JOB_TOKEN_HEADER: &str = "x-hibana-job-token";
pub(crate) async fn claims_from_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<faas_shared::JobClaims, AppError> {
    let token = headers
        .get(JOB_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(FaasError::Unauthorized)?;
    let claims = state
        .signer()
        .verifier()
        .verify(token)
        .map_err(|_| FaasError::Unauthorized)?;
    let now = chrono::Utc::now().timestamp();
    if claims.exp <= now {
        return Err(FaasError::Unauthorized.into());
    }
    if !crate::db::tenant_is_active(state.pool(), &claims.tenant_id).await? {
        return Err(FaasError::Forbidden.into());
    }
    Ok(claims)
}
