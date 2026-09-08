//! Identity management HTTP handlers.
use super::map_unique_violation;
use crate::auth::{hash_token, Principal};
use crate::authz::{require_admin_role, resolve_token_scopes};
use crate::crypto::{generate_secret, hash_password};
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::{new_token_id, new_user_id, FaasError, Role, Scope};
use serde::{Deserialize, Serialize};
use serde_json::json;

// ---------------------------------------------------------------------------
// POST /tenants/{id}/users — テナント内ユーザ作成 (§3.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    pub role: Role,
}

#[derive(Debug, Serialize)]
pub struct CreateUserResponse {
    pub user_id: String,
    pub email: String,
    pub role: Role,
}

/// POST /tenants/{id}/users — テナント内ユーザを作成する（Admin スコープ必須）。
///
/// IDOR 対策(MUST): path の tenant id は呼び出し主体のテナントと一致しなければ
/// ならない。不一致は 404（存在秘匿）。これにより他テナントへのユーザ作成を防ぐ。
pub async fn create_user(
    State(state): State<AppState>,
    principal: Principal,
    Path(path_tenant_id): Path<String>,
    JsonBody(req): JsonBody<CreateUserRequest>,
) -> Result<impl IntoResponse, AppError> {
    // admin ロール必須（scope に加えた多層防御）。
    require_admin_role(principal.role)?;
    // 自テナント以外は 404（存在秘匿）。
    if path_tenant_id != principal.tenant_id {
        return Err(FaasError::NotFound(format!("tenant '{path_tenant_id}'")).into());
    }

    if req.email.trim().is_empty() {
        return Err(FaasError::InvalidRequest("email must not be empty".into()).into());
    }
    if req.password.is_empty() {
        return Err(FaasError::InvalidRequest("password must not be empty".into()).into());
    }

    let password_hash = hash_password(&req.password)?;
    let user_id = new_user_id();

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &principal.tenant_id).await?;
    db::create_user(
        &mut *tx,
        &user_id,
        &principal.tenant_id,
        req.email.trim(),
        &password_hash,
        req.role,
    )
    .await
    .map_err(|e| map_unique_violation(e, "email already exists in this tenant"))?;
    // §3.7: identity プロビジョニングを記録する（GUC 設定済み → user 行と同一 tx で commit）。
    // パスワード/ハッシュは載せない。target は新 user_id、detail に role のみ。
    db::insert_audit_log(
        &mut *tx,
        &principal.tenant_id,
        principal.actor(),
        "user_created",
        Some(&user_id),
        Some(&json!({ "role": req.role.as_str() })),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(user_id = %user_id, tenant = %principal.tenant_id, "user created");
    Ok((
        StatusCode::CREATED,
        Json(CreateUserResponse {
            user_id,
            email: req.email,
            role: req.role,
        }),
    ))
}

// ---------------------------------------------------------------------------
// POST /tokens — API トークン発行 (§3.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    /// トークンを紐づける対象ユーザ（同一テナント内）。
    pub user_id: String,
    /// 要求スコープ。空なら caller ∩ 対象ロール上限を既定付与。
    #[serde(default)]
    pub scopes: Vec<Scope>,
    /// 人間可読ラベル（任意）。
    #[serde(default)]
    pub name: Option<String>,
    /// 失効までの秒数（任意。既定 1 時間）。
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    /// 平文 opaque secret。**一度だけ**返す。
    ///
    /// M7-0 (§5.1): `Redacted` は `Serialize` を実装しないので、平文で返すには `expose_once` を
    /// 明示する必要がある（この属性の grep が「意図的に秘密を返す API」の全一覧になる）。
    #[serde(serialize_with = "hibana_shared::expose_once")]
    pub token: hibana_shared::Redacted<String>,
    pub token_id: String,
    pub scopes: Vec<Scope>,
    pub expires_at: String,
}

/// 発行トークンの既定 TTL（秒）。
const DEFAULT_TOKEN_TTL_SECS: i64 = 3600;
/// 発行トークンの最大 TTL（秒, 30 日）。
const MAX_TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;

/// POST /tokens — 対象ユーザ向けに API トークンを発行する（Admin スコープ必須）。
///
/// 権限ceiling(MUST): 付与スコープは
///   要求 ∩ 呼び出し主体スコープ ∩ 対象ユーザのロール上限
/// に制限する（default-open 禁止）。要求が caller / 上限を超えれば 403。
/// IDOR 対策: 対象ユーザは呼び出し主体のテナント内に限定（テナント外は 404）。
pub async fn create_token(
    State(state): State<AppState>,
    principal: Principal,
    JsonBody(req): JsonBody<CreateTokenRequest>,
) -> Result<impl IntoResponse, AppError> {
    // admin ロール必須（scope に加えた多層防御）。
    require_admin_role(principal.role)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &principal.tenant_id).await?;

    // 対象ユーザは同一テナント内に存在しなければならない（不在/外部は 404）。
    let target_role_str = db::find_user_role(&mut *tx, &principal.tenant_id, &req.user_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("user '{}'", req.user_id)))?;
    let target_role = db::parse_role(Some(&target_role_str))
        .ok_or_else(|| FaasError::Internal("user has invalid role".into()))?;

    // 権限 ceiling 計算（昇格防止）。要求 > caller or > 上限 は 403。
    let scopes = resolve_token_scopes(&req.scopes, &principal.scopes, target_role)?;
    let scope_strs: Vec<String> = scopes.iter().map(|s| s.as_str().to_string()).collect();

    // TTL を [1, MAX] にクランプする。
    let ttl = req
        .ttl_secs
        .unwrap_or(DEFAULT_TOKEN_TTL_SECS)
        .clamp(1, MAX_TOKEN_TTL_SECS);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(ttl);

    let secret = generate_secret();
    let token_hash = hash_token(&secret);
    let token_id = new_token_id();

    db::create_token(
        &mut *tx,
        &token_id,
        &principal.tenant_id,
        Some(&req.user_id),
        &token_hash,
        &scope_strs,
        req.name.as_deref(),
        expires_at,
    )
    .await?;

    // §3.7: トークン発行を audit_logs に記録する（GUC 設定済み → token 行と同一 tx で
    // アトミックに commit）。actor は発行主体、target は新トークン id。生 secret は載せない。
    db::insert_audit_log(
        &mut *tx,
        &principal.tenant_id,
        principal.actor(),
        "token_issued",
        Some(&token_id),
        Some(&json!({ "scopes": scope_strs, "user_id": req.user_id })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(token_id = %token_id, tenant = %principal.tenant_id, "token issued");
    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            token: hibana_shared::Redacted::new(secret),
            token_id,
            scopes,
            expires_at: expires_at.to_rfc3339(),
        }),
    ))
}

// ---------------------------------------------------------------------------
// DELETE /tokens/{id} — トークン失効 (§3.3)
// ---------------------------------------------------------------------------

/// DELETE /tokens/{id} — トークンを失効させる（Admin スコープ必須）。
///
/// IDOR 対策(MUST): 必ず呼び出し主体のテナントでスコープする。テナント外/不在は
/// 404（存在秘匿）。既に失効済みは冪等に 204。
pub async fn revoke_token(
    State(state): State<AppState>,
    principal: Principal,
    Path(token_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    // admin ロール必須（scope に加えた多層防御）。
    require_admin_role(principal.role)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &principal.tenant_id).await?;

    let affected = db::revoke_token(&mut *tx, &principal.tenant_id, &token_id).await?;
    if affected == 0 {
        // 自テナントに存在するが既失効なら 204（冪等）。存在しなければ 404（秘匿）。
        let exists = db::token_exists(&mut *tx, &principal.tenant_id, &token_id).await?;
        tx.commit().await?;
        if exists {
            return Ok(StatusCode::NO_CONTENT);
        }
        return Err(FaasError::NotFound(format!("token '{token_id}'")).into());
    }

    // §3.7: 実際に失効した（affected > 0）ときだけ audit_logs に記録する（GUC 設定済み →
    // 失効と同一 tx でアトミックに commit）。冪等な再失効（affected == 0）は記録しない。
    db::insert_audit_log(
        &mut *tx,
        &principal.tenant_id,
        principal.actor(),
        "token_revoked",
        Some(&token_id),
        None,
    )
    .await?;

    tx.commit().await?;

    tracing::info!(token_id = %token_id, tenant = %principal.tenant_id, "token revoked");
    Ok(StatusCode::NO_CONTENT)
}
