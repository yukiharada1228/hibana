//! Secrets Manager の HTTP ハンドラ (M7c, §10 / §15)。
//!
//! **このファイルは `scripts/rls-lint.sh` の検査 (4) の allowlist に入る**（平文を扱う数少ない場所）。
//!
//! ## write-only 設計
//!
//! 値を返す経路を**コードに作らない**。`GET /secrets` はメタデータ（名前・世代・値の有無）だけを返し、
//! 復号した値が HTTP 応答へ載る型経路そのものを存在させない。運用に必要な `kek_kid` / `value_len` は
//! admin 専用の `GET /secrets/keys` に分離する —— `value_len` は**平文長のオラクル**であり、
//! 短い PIN やパターンの決まった資格情報では有意な情報になるため read スコープには見せない。
//!
//! ## 二重ガード
//!
//! 書き込み系は `admin` スコープ（ルータ）+ `require_admin_role`（ハンドラ）。根拠は §4.4 が
//! 「capability の付与（承認）は admin スコープを要する (MUST)」と規定し、outbound 認証情報の
//! 管理はこの「付与」に含まれると解するため（`create_user` / `create_token` と同じ多層防御）。
//!
//! ## 監査
//!
//! `detail` は `secrets::audit_detail`（**値を受け取らないシグネチャ**）だけを通す。生値が
//! `db::insert_audit_log` に到達する型経路を消すのが目的。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use faas_shared::FaasError;

use crate::auth::Principal;
use crate::authz::require_admin_role;
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::secrets;
use crate::state::AppState;

/// `sec_*` の採番（`faas_shared` の ID 採番作法に合わせた不透明 ID）。
fn new_secret_id() -> String {
    faas_shared::new_secret_id()
}

/// 値の受け取り。**`Debug` を derive しない**（`JsonBody` の rejection ログ等に載せないため）。
#[derive(Deserialize)]
pub struct PutSecretRequest {
    /// 平文の値。応答にも監査にもログにも出さない。
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct PutSecretResponse {
    pub name: String,
    pub version: i32,
}

/// `GET /secrets`（read）で返すメタデータ。**値も value_len も kek_kid も含まない**。
#[derive(Debug, Serialize)]
pub struct SecretMeta {
    pub name: String,
    pub version: i32,
    /// 値が設定されているか。`false` になるのは版台帳が壊れている異常時のみ。
    pub has_value: bool,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /secrets/keys`（admin）で返す運用情報。`kek_kid` / `value_len` は**ここだけ**。
#[derive(Debug, Serialize)]
pub struct SecretKeyInfo {
    pub name: String,
    pub version: i32,
    pub kek_kid: String,
    pub value_len: i32,
}

/// component の存在確認（不在は 404。存在秘匿の既存作法どおり 403 にしない）。
async fn require_component(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    component_id: &str,
) -> Result<(), AppError> {
    db::find_component_by_id(&mut *tx, tenant, component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;
    Ok(())
}

/// secret 名と平文値の受付検証（純関数部分は `faas_shared` の定数を共有）。
fn validate_secret_input(name: &str, value: &str) -> Result<(), FaasError> {
    if !faas_shared::is_valid_env_key(name) {
        return Err(FaasError::InvalidRequest(format!(
            "invalid secret name '{name}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
            faas_shared::MAX_ENV_KEY_LEN - 1
        )));
    }
    if value.is_empty() {
        return Err(FaasError::InvalidRequest("value must not be empty".into()));
    }
    if value.len() > faas_shared::MAX_ENV_VALUE_BYTES {
        // **値そのものは絶対にメッセージへ入れない**（長さだけ）。
        return Err(FaasError::InvalidRequest(format!(
            "secret value exceeds {} bytes",
            faas_shared::MAX_ENV_VALUE_BYTES
        )));
    }
    Ok(())
}

/// config 側に同名キーがあれば 409（どちらが勝つかを作らない, §3.2 / §4.3）。
///
/// 「secret を上書きしたつもりの平文 config が注入される」事故を構造的に防ぐ。
async fn reject_config_key_collision(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    component_id: &str,
    name: &str,
) -> Result<(), AppError> {
    let configs = db::list_function_configs(&mut *tx, tenant, component_id).await?;
    if configs.iter().any(|c| c.key == name) {
        return Err(FaasError::Conflict(format!(
            "config key '{name}' already exists for this component; \
             delete it first (a name must be either config or secret, never both)"
        ))
        .into());
    }
    Ok(())
}

/// 値を封筒暗号化して新しい版を書き込む共通処理（`PUT` と `rotate` が共有する）。
///
/// 新規なら version=1 で `function_secrets` を作り、既存なら `current_version + 1` を追記して
/// ポインタを前進させる。**版台帳は追記専用**なので更新は常に INSERT になる。
async fn write_secret_version(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    principal: &Principal,
    component_id: &str,
    name: &str,
    value: &str,
    reason: &str,
) -> Result<(i32, bool), AppError> {
    let tenant = &principal.tenant_id;
    let keyring = state.secret_keyring();

    let existing = db::find_live_secret_by_name(&mut *tx, tenant, component_id, name).await?;

    let (secret_id, version, created) = match &existing {
        Some(meta) => (meta.id.clone(), meta.current_version + 1, false),
        None => (new_secret_id(), 1, true),
    };

    // 封筒暗号化。AAD に (tenant, component, name) と (tenant, secret_id, version, kid) を束縛する。
    let envelope = secrets::encrypt(
        keyring,
        tenant,
        component_id,
        &secret_id,
        name,
        version,
        value.as_bytes(),
    )
    .map_err(map_secret_error)?;

    if created {
        db::insert_secret_meta(&mut *tx, tenant, &secret_id, component_id, name, version).await?;
    }
    db::insert_secret_version(
        &mut *tx,
        tenant,
        &secret_id,
        version,
        &envelope,
        reason,
        principal.user_id.as_deref(),
    )
    .await?;
    if !created {
        db::bump_secret_current_version(&mut *tx, tenant, &secret_id, version).await?;
    }

    Ok((version, created))
}

/// `SecretError` を HTTP へ写像する。**ボディには理由も値も出さない**（一律 500）。
///
/// 安定 reason は内部ログにだけ出す（`error.rs` の 5xx redaction と同じ思想）。
fn map_secret_error(e: secrets::SecretError) -> AppError {
    tracing::error!(reason = e.reason(), "secret cryptographic operation failed");
    FaasError::Internal(String::new()).into()
}

/// PUT /components/{id}/secrets/{name} — 値を設定する（新規 201 / 更新 200）。
pub async fn put_secret(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, name)): Path<(String, String)>,
    JsonBody(req): JsonBody<PutSecretRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    validate_secret_input(&name, &req.value)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    require_component(&mut tx, tenant, &component_id).await?;
    reject_config_key_collision(&mut tx, tenant, &component_id, &name).await?;

    // 新規かどうかを先に確定して監査 reason を決める（write_secret_version も同じ判定を行うが、
    // 判定は同一 tx 内の同一スナップショットなので食い違わない）。
    let is_new = db::find_live_secret_by_name(&mut *tx, tenant, &component_id, &name)
        .await?
        .is_none();
    let reason = if is_new { "create" } else { "rotate" };

    let (version, created) = write_secret_version(
        &state,
        &mut tx,
        &principal,
        &component_id,
        &name,
        &req.value,
        reason,
    )
    .await?;

    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "secret_updated",
        Some(&component_id),
        Some(&secrets::audit_detail(
            &name,
            version,
            if created { "create" } else { "rotate" },
        )),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, secret = %name, version, created, "secret version written");

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(PutSecretResponse { name, version })))
}

/// POST /components/{id}/secrets/{name}/rotate — 値を差し替える。
///
/// 実装は `put_secret` と同じだが、**監査 action を分ける**（意図が違う操作を監査上で区別する）。
pub async fn rotate_secret(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, name)): Path<(String, String)>,
    JsonBody(req): JsonBody<PutSecretRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    validate_secret_input(&name, &req.value)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    require_component(&mut tx, tenant, &component_id).await?;

    // rotate は既存が前提（不在は 404）。
    if db::find_live_secret_by_name(&mut *tx, tenant, &component_id, &name)
        .await?
        .is_none()
    {
        return Err(
            FaasError::NotFound(format!("secret '{name}' of component '{component_id}'")).into(),
        );
    }

    let (version, _) = write_secret_version(
        &state,
        &mut tx,
        &principal,
        &component_id,
        &name,
        &req.value,
        "rotate",
    )
    .await?;

    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "secret_rotated",
        Some(&component_id),
        Some(&secrets::audit_detail(&name, version, "rotate")),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, secret = %name, version, "secret rotated");
    Ok(Json(PutSecretResponse { name, version }))
}

/// DELETE /components/{id}/secrets/{name} — soft delete（204）。
///
/// 版台帳は追記専用なので残る（監査上も残す）。生存行の部分 UNIQUE index から外れるため
/// **同名で作り直せる**（侵害された資格情報の入れ替えという最も基本のインシデント対応）。
pub async fn delete_secret(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    require_component(&mut tx, tenant, &component_id).await?;

    let meta = db::find_live_secret_by_name(&mut *tx, tenant, &component_id, &name)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("secret '{name}' of component '{component_id}'"))
        })?;

    db::soft_delete_secret(&mut *tx, tenant, &meta.id).await?;

    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "secret_deleted",
        Some(&component_id),
        Some(&secrets::audit_detail(
            &name,
            meta.current_version,
            "delete",
        )),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, secret = %name, "secret soft-deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// GET /components/{id}/secrets — **メタデータのみ**（read スコープ）。
pub async fn list_secrets(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    require_component(&mut tx, tenant, &component_id).await?;

    let rows = db::list_secrets_meta(&mut *tx, tenant, &component_id).await?;
    tx.commit().await?;

    let out: Vec<SecretMeta> = rows
        .into_iter()
        .map(|m| SecretMeta {
            name: m.name,
            version: m.current_version,
            has_value: m.current_version >= 1,
            updated_at: m.updated_at,
        })
        .collect();

    Ok(Json(json!({ "secrets": out })))
}

/// GET /components/{id}/secrets/keys — 運用向け（admin）。`kek_kid` / `value_len` はここだけ。
pub async fn list_secret_keys(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    require_component(&mut tx, tenant, &component_id).await?;

    let metas = db::list_secrets_meta(&mut *tx, tenant, &component_id).await?;
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        // 版台帳から現行世代の暗号材料メタだけを引く（**平文は復号しない**）。
        if let Some(env) =
            db::find_secret_version(&mut *tx, tenant, &m.id, m.current_version).await?
        {
            out.push(SecretKeyInfo {
                name: m.name,
                version: m.current_version,
                kek_kid: env.kek_kid,
                value_len: env.value_len,
            });
        }
    }
    tx.commit().await?;

    Ok(Json(json!({ "secrets": out })))
}
