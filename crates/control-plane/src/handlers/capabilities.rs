//! Capabilities management HTTP handlers.
use crate::auth::Principal;
use crate::authz::require_admin_role;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::{db, validation};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::FaasError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Serialize)]
pub struct GetCapabilitiesResponse {
    pub component_id: String,
    pub version: String,
    /// 注入が承認された env 名（M7b/M9c）。
    pub env: Vec<String>,
    /// 承認された egress 先（host:port, M9c）。
    pub net_allow_outbound: Vec<String>,
}

/// GET /components/{id}/versions/{version}/capabilities — 現在の承認内容を返す（Read）。
///
/// **値は返さない**（env の名前と egress 先のみ）。CLI が「既存を保ったまま名前を足す」
/// マージのために読む用途（承認 PUT は全置換なので、GET してマージしてから PUT する）。
pub async fn get_capabilities(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let version_id = db::find_version_id(&mut *tx, tenant, &component_id, &version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("version '{version}' of component '{component_id}'"))
        })?;
    let current = db::version_capabilities(&mut *tx, tenant, &version_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    let caps = validation::parse_capabilities(&current);
    Ok(Json(GetCapabilitiesResponse {
        component_id,
        version,
        env: caps.env.into_iter().collect(),
        net_allow_outbound: caps.net_allow_outbound.into_iter().collect(),
    }))
}

/// Legacy env grants must not mutate the environment of an existing version.
pub async fn approve_capability_env() -> Result<(), AppError> {
    Err(FaasError::Conflict("environment bindings are versioned; authorize Secret deploy-access, then deploy a new version".into()).into())
}

#[derive(Debug, Deserialize)]
pub struct ApproveCapabilityEgressRequest {
    /// 許可する outbound 先（`host:port`）の**全置換**リスト（空配列 = egress deny-all へ戻す）。
    pub allow_outbound: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApproveCapabilityEgressResponse {
    pub component_id: String,
    pub version: String,
    pub version_id: String,
    pub allow_outbound: Vec<String>,
}

/// PUT /components/{id}/versions/{version}/capabilities/egress — egress allowlist を承認する（admin, M9c）。
///
/// admin スコープ + admin ロールの二重ガード。envは保持し、egressだけを更新する。
/// `wasi:sockets/*` の import は baseline で許可されるが、実際の outbound はこの allowlist が
/// 非空のときだけ worker の `socket_addr_check` が通す。ここで承認された `host:port` を
/// `capabilities.net_allow_outbound` へ**全置換**で書く。空配列は egress を deny-all に戻す。
///
/// **deploy スコープの upload 経路ではこの値を書けない**（upload は常に空で保存する）。
/// egress は admin 専用の管理操作である（deploy トークンが自分で外部到達を承認できてはならない）。
pub async fn approve_capability_egress(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
    JsonBody(req): JsonBody<ApproveCapabilityEgressRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    // 各エントリを host:port として検証し、正規化する（重複は BTreeSet が畳む）。
    // 1 つでも不正なら 400 で全体を拒否する（部分承認しない）。
    let mut approved = std::collections::BTreeSet::new();
    for raw in &req.allow_outbound {
        let ep = hibana_shared::egress::parse_egress_endpoint(raw).map_err(|e| {
            FaasError::InvalidRequest(format!("invalid egress endpoint '{raw}': {e}"))
        })?;
        approved.insert(format!("{}:{}", ep.host, ep.port));
    }

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    let version_id = db::find_version_id(&mut *tx, tenant, &component_id, &version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("version '{version}' of component '{component_id}'"))
        })?;

    // 既存の imports / env は保持し、net_allow_outbound だけを差し替える。
    let current = db::version_capabilities(&mut *tx, tenant, &version_id)
        .await?
        .unwrap_or(Value::Null);
    let mut caps = validation::parse_capabilities(&current);
    caps.net_allow_outbound = approved.clone();

    if !db::set_version_capabilities(&mut *tx, tenant, &version_id, &caps.to_json()).await? {
        return Err(FaasError::NotFound(format!(
            "version '{version}' of component '{component_id}'"
        ))
        .into());
    }

    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "capability_egress_approved",
        Some(&version_id),
        Some(&json!({
            "component_id": component_id,
            "version": version,
            "allow_outbound": approved.iter().collect::<Vec<_>>(),
        })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(
        %component_id,
        %version,
        approved_egress_count = approved.len(),
        "capability egress allowlist approved"
    );

    Ok(Json(ApproveCapabilityEgressResponse {
        component_id,
        version,
        version_id,
        allow_outbound: approved.into_iter().collect(),
    }))
}
