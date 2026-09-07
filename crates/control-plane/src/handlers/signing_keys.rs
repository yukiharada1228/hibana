//! Signing keys management HTTP handlers.
use crate::auth::Principal;
use crate::authz::require_admin_role;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::{db, signing};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use faas_shared::FaasError;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ---------------------------------------------------------------------------
// M9a: Component 署名鍵の管理 + 署名必須ポリシー（§6.2 / §15 M9）
//
// すべて admin スコープ（ルータ）+ admin ロール（require_admin_role）の二重ガード。
// **秘密鍵はプラットフォームに一切渡らない**（テナントが手元で署名し、公開鍵だけ登録する）。
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RegisterSigningKeyRequest {
    /// Ed25519 公開鍵（base64url, パディング無し, 32 バイト）。
    pub public_key: String,
}

#[derive(Debug, Serialize)]
pub struct SigningKeyView {
    pub key_id: String,
    pub public_key: String,
    pub status: String,
}

/// PUT /admin/signing-keys/{key_id} — 署名鍵を登録 / 差し替える（admin）。
///
/// 同一 key_id への再 PUT は公開鍵を上書きし status を active に戻す（ローテーション時の再登録）。
pub async fn register_signing_key(
    State(state): State<AppState>,
    principal: Principal,
    Path(key_id): Path<String>,
    JsonBody(req): JsonBody<RegisterSigningKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    // 公開鍵として妥当（32 バイトの Ed25519 点）であることを登録時に検証する。
    // 壊れた鍵を DB に入れると、後の署名検証で「使える鍵が 1 つも無い」に化ける。
    signing::validate_public_key_b64url(&req.public_key)
        .map_err(|e| FaasError::InvalidRequest(format!("invalid public_key: {}", e.reason())))?;

    if key_id.trim().is_empty() || key_id.len() > 128 {
        return Err(FaasError::InvalidRequest("key_id must be 1..=128 chars".into()).into());
    }

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    db::upsert_signing_key(
        &mut *tx,
        tenant,
        &key_id,
        &req.public_key,
        principal.user_id.as_deref(),
    )
    .await?;
    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "signing_key_registered",
        Some(&key_id),
        // 公開鍵は秘密ではないが、監査には key_id だけ残す（応答で公開鍵は返す）。
        Some(&json!({ "key_id": key_id })),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(%key_id, "component signing key registered");
    Ok(Json(SigningKeyView {
        key_id,
        public_key: req.public_key,
        status: "active".into(),
    }))
}

/// GET /admin/signing-keys — 登録済み署名鍵の一覧（admin）。
pub async fn list_signing_keys(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let keys = db::list_signing_keys(&mut *tx, tenant).await?;
    tx.commit().await?;

    let out: Vec<SigningKeyView> = keys
        .into_iter()
        .map(|k| SigningKeyView {
            key_id: k.key_id,
            public_key: k.public_key,
            status: k.status,
        })
        .collect();
    Ok(Json(out))
}

/// DELETE /admin/signing-keys/{key_id} — 鍵を retire する（admin）。
///
/// 物理削除ではなく retire（status='retired'）。過去にその鍵で署名された version の再検証を
/// 壊さないため（M7c secret の KEK と同じ思想）。不在は 404。
pub async fn retire_signing_key(
    State(state): State<AppState>,
    principal: Principal,
    Path(key_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let found = db::retire_signing_key(&mut *tx, tenant, &key_id).await?;
    if !found {
        return Err(FaasError::NotFound(format!("signing key '{key_id}'")).into());
    }
    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "signing_key_retired",
        Some(&key_id),
        Some(&json!({ "key_id": key_id })),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(%key_id, "component signing key retired");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct SigningPolicyRequest {
    /// true にすると署名必須。false で従来どおり（署名が有れば検証するが必須ではない）。
    pub require_signed_components: bool,
}

#[derive(Debug, Serialize)]
pub struct SigningPolicyResponse {
    pub require_signed_components: bool,
}

/// PUT /admin/signing-policy — 署名必須ポリシーを切り替える（admin）。
///
/// true にする前に**有効な鍵を登録し、既存 active version が署名済みであること**を
/// 運用者が確認する必要がある（true 化後は署名なしの再アップロードができなくなる）。
pub async fn set_signing_policy(
    State(state): State<AppState>,
    principal: Principal,
    JsonBody(req): JsonBody<SigningPolicyRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let ok =
        db::set_require_signed_components(&mut *tx, tenant, req.require_signed_components).await?;
    if !ok {
        return Err(FaasError::NotFound(format!("tenant '{tenant}'")).into());
    }
    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "signing_policy_updated",
        None,
        Some(&json!({ "require_signed_components": req.require_signed_components })),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(
        require_signed = req.require_signed_components,
        "component signing policy updated"
    );
    Ok(Json(SigningPolicyResponse {
        require_signed_components: req.require_signed_components,
    }))
}
