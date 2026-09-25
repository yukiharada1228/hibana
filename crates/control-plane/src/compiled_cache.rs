//! Internal-only publication of authenticated, disposable native code.
use crate::{error::AppError, state::AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use futures::StreamExt;
use hibana_shared::{compiled_cache as protocol, FaasError};
use std::time::Duration;

pub(crate) fn runtime(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(protocol::RUNTIME_HEADER)?
        .to_str()
        .ok()
        .filter(|id| protocol::valid_id(id))
}

pub(crate) async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, AppError> {
    let claims = crate::preparation::authorize(&state, &headers).await?;
    let runtime = runtime(&headers).ok_or(FaasError::Unauthorized)?;
    let auth = state
        .storage()
        .compiled_auth
        .as_ref()
        .ok_or(FaasError::Unavailable)?;
    // Authenticate headers and reserve memory before polling any request body.
    let _slot = state
        .storage()
        .compiled_uploads
        .try_acquire()
        .map_err(|_| FaasError::Unavailable)?;
    let size = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if size.is_some_and(|n| n > protocol::MAX_OBJECT_BYTES) {
        return Err(FaasError::InvalidRequest("Compiled cache object too large".into()).into());
    }
    let bytes = tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = Vec::with_capacity(size.unwrap_or(0));
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| FaasError::InvalidRequest("Invalid cache body".into()))?;
            if bytes.len().saturating_add(chunk.len()) > protocol::MAX_OBJECT_BYTES {
                return Err(FaasError::InvalidRequest(
                    "Compiled cache object too large".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, FaasError>(bytes)
    })
    .await
    .map_err(|_| FaasError::Unavailable)??;
    auth.verify(runtime, &claims.sha256, &bytes)
        .map_err(|_| FaasError::Unauthorized)?;
    let key = protocol::object_key(runtime, &claims.sha256).map_err(|_| FaasError::Unauthorized)?;
    state
        .storage()
        .put_compiled(state.pool(), &key, bytes)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
