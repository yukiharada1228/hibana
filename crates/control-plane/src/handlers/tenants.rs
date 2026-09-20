//! Tenants management HTTP handlers.
use super::map_unique_conflict;
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::{new_tenant_id, new_user_id, FaasError};
use sea_orm::TransactionTrait as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// POST /admin/tenants — テナント作成 (bootstrap system-admin, §3.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTenantRequest {
    /// グローバル一意な公開 URL の DNS ラベル。
    pub slug: String,
    pub name: String,
    /// 最初の admin ユーザの email（このテナント内）。
    pub admin_email: String,
    pub admin_oidc_subject: String,
}

#[derive(Debug, Serialize)]
pub struct CreateTenantResponse {
    pub tenant_id: String,
    pub slug: String,
    pub name: String,
    /// 作成された最初の admin ユーザ。以後は OIDC でログインする。
    pub admin_user_id: String,
    pub admin_email: String,
}

/// POST /admin/tenants — bootstrap トークンで保護される system-admin パス。
///
/// 認証 middleware の対象外（ルータで除外）。代わりに `Authorization: Bearer
/// ${BOOTSTRAP_ADMIN_TOKEN}` を**定数時間**で照合する。不一致/欠損は 401。
/// tenant_id は uuid-simple 由来で subject-safe。
pub async fn create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<CreateTenantRequest>,
) -> Result<impl IntoResponse, AppError> {
    // bootstrap トークン照合（system-admin gate）。
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if !bootstrap_token_matches(provided, state.bootstrap_admin_token()) {
        return Err(FaasError::Unauthorized.into());
    }

    if req.slug.trim().is_empty() || req.name.trim().is_empty() {
        return Err(FaasError::InvalidRequest("slug and name must not be empty".into()).into());
    }
    let slug = req.slug.trim();
    if !crate::public_apps::valid_label(slug) {
        return Err(FaasError::InvalidRequest(
            "slug must be 1-63 lowercase ASCII letters, digits or hyphens, with no leading or trailing hyphen".into(),
        ).into());
    }
    if req.admin_email.trim().is_empty() {
        return Err(FaasError::InvalidRequest("admin_email must not be empty".into()).into());
    }
    super::identity::validate_subject(&req.admin_oidc_subject)?;

    // テナント + 最初の admin ユーザを 1 トランザクションで作成（§9 bootstrap）。
    // M3b: bootstrap は Principal を持たない唯一の書き込み経路。users は FORCE RLS 下に
    // あるため、生成直後の tenant_id で同一 tx に GUC を設定してから bootstrap_tenant を
    // 呼ぶ（users INSERT の WITH CHECK を通すため。tenants には RLS は無い）。
    let tenant_id = new_tenant_id();
    let admin_user_id = new_user_id();
    let tx = state.pool().begin().await.map_err(AppError::from)?;
    db::set_tenant_guc(&tx, &tenant_id)
        .await
        .map_err(AppError::from)?;
    db::bootstrap_tenant(
        &tx,
        &tenant_id,
        slug,
        req.name.trim(),
        &admin_user_id,
        req.admin_email.trim(),
        &state.auth_config().issuer,
        &req.admin_oidc_subject,
    )
    .await
    .map_err(|e| map_unique_conflict(e, "tenant slug already exists"))?;
    // §3.7: 最高権限の identity プロビジョニング。GUC は新 tenant_id に設定済みなので
    // WITH CHECK を通る。bootstrap 主体由来であることを示し、target は新 tenant_id。
    db::insert_audit_log(
        &tx,
        &tenant_id,
        Some("bootstrap"),
        "tenant_created",
        Some(&tenant_id),
        Some(&json!({ "slug": slug, "admin_user_id": admin_user_id })),
    )
    .await
    .map_err(AppError::from)?;
    tx.commit().await.map_err(AppError::from)?;

    tracing::info!(tenant_id = %tenant_id, slug, admin_user_id = %admin_user_id, "tenant bootstrapped");
    Ok((
        StatusCode::CREATED,
        Json(CreateTenantResponse {
            tenant_id,
            slug: slug.to_owned(),
            name: req.name,
            admin_user_id,
            admin_email: req.admin_email,
        }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// M10 follow-up: テナント status / quotas の platform 管理 API（§8 / §9）
//
// **create_tenant と同じ bootstrap トークン gate**（= platform-admin。テナント admin ではない）。
// これらは任意テナントを対象にする cross-tenant 操作なので、principal.tenant_id ではなく path の
// {tenant_id} を対象にし、認証はテナント admin スコープではなく bootstrap トークンで行う
// （テナント自身が自分の suspend 解除やクォータ引き上げをできてはならない）。
// ルータの**認証 middleware の外**（create_tenant と同じ非認証グループ）へマウントする。
// ---------------------------------------------------------------------------

/// リクエストから bootstrap トークンを取り出して照合する（platform-admin gate）。不一致は Unauthorized。
pub(crate) fn require_bootstrap_admin(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<(), FaasError> {
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if bootstrap_token_matches(provided, state.bootstrap_admin_token()) {
        Ok(())
    } else {
        Err(FaasError::Unauthorized)
    }
}

#[derive(Debug, Deserialize)]
pub struct SetTenantStatusRequest {
    /// "active" | "suspended"。
    pub status: String,
}

/// PUT /admin/tenants/{tenant_id}/status — テナントを suspend / 再有効化する（platform admin）。
pub async fn set_tenant_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
    JsonBody(req): JsonBody<SetTenantStatusRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_bootstrap_admin(&headers, &state)?;

    let status = req.status.trim();
    if status != "active" && status != "suspended" {
        return Err(
            FaasError::InvalidRequest("status must be 'active' or 'suspended'".into()).into(),
        );
    }

    // Apply the change and its audit together, under the target tenant's GUC.
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &tenant_id).await?;
    if !db::set_tenant_status(&tx, &tenant_id, status).await? {
        return Err(FaasError::NotFound(format!("tenant '{tenant_id}'")).into());
    }

    db::insert_audit_log(
        &tx,
        &tenant_id,
        Some("bootstrap"),
        "tenant_status_updated",
        Some(&tenant_id),
        Some(&json!({ "status": status })),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(%tenant_id, status, "tenant status updated");
    Ok(Json(json!({ "tenant_id": tenant_id, "status": status })))
}

#[derive(Debug, Deserialize)]
pub struct SetTenantQuotasRequest {
    #[serde(default)]
    pub invoke_rate_per_sec: Option<u64>,
    #[serde(default)]
    pub invoke_burst: Option<u64>,
    #[serde(default)]
    pub max_concurrent_executions: Option<u64>,
}

/// PUT /admin/tenants/{tenant_id}/quotas — クォータ上書きを全置換する（platform admin, §8）。
///
/// 省略 / null のフィールドは「グローバル既定を継承」を意味する（`load_tenant_status_and_quotas`
/// が緩く解釈する）。ここでは known フィールドだけを正規化して保存し、未知キーが混入した
/// JSONB を作らない。実効値は admission 側で推奨上限にクランプされる（`AdmissionConfig::resolve_for_tenant`）
/// ので、ここでは範囲チェックはせず「保存できる形」だけを保証する。
pub async fn set_tenant_quotas(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
    JsonBody(req): JsonBody<SetTenantQuotasRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_bootstrap_admin(&headers, &state)?;

    // Some のフィールドだけを持つ正規化 JSON を作る（None は入れない = 既定継承）。
    let mut obj = serde_json::Map::new();
    if let Some(v) = req.invoke_rate_per_sec {
        obj.insert("invoke_rate_per_sec".into(), json!(v));
    }
    if let Some(v) = req.invoke_burst {
        obj.insert("invoke_burst".into(), json!(v));
    }
    if let Some(v) = req.max_concurrent_executions {
        obj.insert("max_concurrent_executions".into(), json!(v));
    }
    let quotas = Value::Object(obj);

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &tenant_id).await?;
    if !db::set_tenant_quotas(&tx, &tenant_id, &quotas).await? {
        return Err(FaasError::NotFound(format!("tenant '{tenant_id}'")).into());
    }

    db::insert_audit_log(
        &tx,
        &tenant_id,
        Some("bootstrap"),
        "tenant_quotas_updated",
        Some(&tenant_id),
        Some(&quotas),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(%tenant_id, quotas = %quotas, "tenant quotas updated");
    Ok(Json(json!({ "tenant_id": tenant_id, "quotas": quotas })))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// bootstrap トークンを定数時間で照合する（長さ込み）。
///
/// 空の期待値（未設定）は常に false（誤って全許可しない）。
pub(super) fn bootstrap_token_matches(provided: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    // 長さが異なっても早期 return せず、固定回数で比較する。
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}
