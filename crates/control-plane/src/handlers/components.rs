//! Components management HTTP handlers.
use super::map_unique_violation;
use crate::auth::Principal;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::{db, signing, validation};
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use hibana_database::prelude::*;
use hibana_shared::{new_component_id, new_version_id, FaasError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// POST /components
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateComponentRequest {
    /// テナント内一意の Component 名。
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CreateComponentResponse {
    pub component_id: String,
    pub name: String,
}

/// POST /components — Component メタデータのみ作成 (M2: §6.2)。
///
/// 初期 version は作らない（active_version_id は NULL）。実体は
/// POST /components/{id}/versions で投入する。
pub async fn create_component(
    State(state): State<AppState>,
    principal: Principal,
    JsonBody(req): JsonBody<CreateComponentRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    if !crate::public_apps::valid_label(&req.name) {
        return Err(FaasError::InvalidRequest(
            "name must be 1-63 lowercase ASCII letters, digits or hyphens, with no leading or trailing hyphen".into(),
        ).into());
    }

    let component_id = new_component_id();

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    db::create_component(&tx, tenant, &component_id, &req.name)
        .await
        .map_err(|error| {
            if super::is_unique_violation(&error) {
                FaasError::Conflict("component name already exists".into()).into()
            } else {
                AppError::from(error)
            }
        })?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateComponentResponse {
            component_id,
            name: req.name,
        }),
    ))
}

// ---------------------------------------------------------------------------
// POST /components/{component_id}/versions  (multipart/form-data, §6.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UploadVersionResponse {
    pub public_url: Option<String>,
    pub version_id: String,
    pub version: String,
}

/// POST /components/{component_id}/versions — アップロード+検証+デプロイ (§6.2)。
///
/// multipart フィールド:
/// - `version` (必須): 1..=128 ASCII characters, starting with a letter or digit;
///   remaining characters may also include '.', '_', '+', '-'.
/// - `wasm` (必須): wasm component バイナリ
/// - `signature` (任意): Wasm本体のsha256に対するdetached Ed25519署名（base64url）。
/// - `resource_limits` (任意): JSON (hibana_shared::ResourceLimits)
/// - `vars` (任意): 平文環境変数のJSONマップ。既定は空。
/// - `secrets` (任意): 管理者が配備利用を許可したSecret名のJSON配列。既定は空。
/// - `activate` (任意): JSON boolean。既定はtrue。
/// - `ingress` (任意): JSON boolean。activate=true時だけ指定可。省略時は現状維持。
///
/// §6.2 の順序:
/// 1. 受信ストリーミング中に MAX_WASM_UPLOAD_BYTES を強制（超過は打ち切り→413）
/// 2. validation.rs で隔離検証（Component Model 妥当性）
/// 3. import 許可リスト照合（validation 内）
/// 4. sha256 算出・サイズ確定（validation 内）
/// 5. 検証・保存・Worker準備を完了してからversionを登録し、activate=trueなら公開先を更新
pub async fn upload_version(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    multipart: Multipart,
) -> Result<axum::response::Response, AppError> {
    let tenant = &principal.tenant_id;
    // Retain these guards through validation, storage and publication: no queued
    // upload may keep its payload outside the same bounded reception budget.
    let capacity = state.upload_capacity();
    let Some(_slot) = capacity.reserve() else {
        return Ok(crate::admission::RateLimited::upload_capacity(false).into_response());
    };
    let Some(_tenant_slot) = capacity.reserve_tenant(tenant, 2) else {
        return Ok(crate::admission::RateLimited::upload_capacity(true).into_response());
    };

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // 対象 component の存在確認（active 化先・名前解決のため）。
    let component = db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    tx.commit().await?; // No DB transaction while receiving or validating the upload.

    let super::upload::Upload {
        version,
        wasm_bytes,
        signature,
        resource_limits,
        activate,
        ingress,
        environment,
    } = match super::upload::receive(multipart, state.max_wasm_upload_bytes()).await {
        Ok(upload) => upload,
        Err(response) => return Ok(*response),
    };

    let allowed_env = environment.validate()?;
    if ingress.is_some() && !activate {
        return Err(FaasError::InvalidRequest("ingress requires activate=true".into()).into());
    }

    // (2)(3)(4) 隔離検証 + §4.4 capability strict matching + sha256/サイズ確定。
    //
    // Persist imports validated against Hibana's WASI host contract. Client
    // declarations cannot add host functions or grant runtime permissions.
    let validated = validation::validate_wasm(&wasm_bytes).await?;

    // §4.4 MUST: クライアント宣言値ではなく、承認集合と照合して解決した import を保存する。
    //
    // Plain vars and selected, separately authorized Secrets form the environment.

    // (5) 検証通過後に保存・準備し、versionと公開先を同一トランザクションで登録。
    let version_id = new_version_id();
    // Immutable identity: retries and recreated names must never overwrite an accepted artifact.
    let object_key = format!("{tenant}/versions/{version_id}.wasm");
    let mut reservation = crate::artifact_reservations::Reservation::new(
        &state,
        tenant,
        &version_id,
        &object_key,
        &validated.sha256,
    )
    .await?;
    let result = async {
    state
        .storage()
        .put_object(&object_key, wasm_bytes, "application/wasm")
        .await?;
    reservation.confirm_object();

    // Prepare before acquiring publication locks: the old version keeps serving.
    // This only compiles verified Wasm; the handler is never called and no Secrets are supplied.
    crate::preparation::prepare(&state, tenant, &object_key, &validated.sha256).await?;

    // Recheck the component and signing policy after external I/O. These locks
    // serialize publication with deletion and policy changes, in a short transaction.
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    reservation.lock(&tx).await?;
    let tenant_info = tenants::Entity::find_by_id(tenant.to_owned()).filter(tenants::Column::Status.eq("active")).lock_shared().one(&tx).await?
        .ok_or_else(|| FaasError::NotFound("active tenant".into()))?;
    if !db::lock_component(&tx, tenant, &component_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }

    // M9a (§6.2): 供給網検証 —— テナントが登録した公開鍵での署名検証。
    //
    // deploy トークンは「アップロードの認可」、署名は「本体の真正性」で、**別々の秘密**に
    // 依存させる（片方が漏れても攻撃が成立しない）。ポリシー `require_signed_components`:
    //   - true: 署名が必須。無い / 検証失敗 / 鍵不在は 422（fail-closed）。
    //   - false（既定）: 署名が**有れば**検証する（不正署名は拒否）。無ければ従来どおり通す。
    // どちらでも「署名が付いていて不正」は拒否する（誤検証を黙認しない）。
    //
    // 署名対象は本体そのものではなく `validated.sha256`（検証パイプラインが算出済みの 16 進文字列）。
    let require_signed = db::require_signed_components(&tx, tenant).await?;
    if require_signed || signature.is_some() {
        let rejection = if let Some(sig) = signature.as_deref() {
            let keys = db::list_signing_keys(&tx, tenant)
                .await?
                .into_iter()
                .map(|k| (k.key_id, k.public_key))
                .collect::<Vec<_>>();
            if keys.is_empty() {
                Some(("no_signing_keys_registered", "component signature present/required but no signing keys are registered for this tenant (register one via PUT /admin/signing-keys/{key_id})".to_owned()))
            } else {
                signing::verify_component_signature(&validated.sha256, sig, &keys)
                    .err()
                    .map(|error| (error.reason(), format!("component signature verification failed: {}", error.reason())))
            }
        } else {
            Some(("signature_required",
                "this tenant requires signed components; provide a 'signature' field \
                 (detached Ed25519 over the wasm sha256, base64url)"
                    .to_owned()))
        };
        if let Some((reason, message)) = rejection {
            tracing::warn!(%component_id, reason, require_signed, "component signature rejected");
            db::insert_audit_log(
                &tx,
                tenant,
                principal.actor(),
                "component_signature_rejected",
                Some(&component.id),
                Some(&json!({
                    "version": version,
                    "sha256": validated.sha256,
                    "reason": reason,
                })),
            )
            .await?;
            // Only the rejection audit has been written in this transaction.
            // Commit it before returning the error; publication starts below.
            tx.commit().await?;
            return Err(FaasError::InvalidRequest(message).into());
        }
    }

    let capabilities_json = json!({
        "imports": validated.approved_imports,
        "env": allowed_env,
    });

    let limits_json = serde_json::to_value(resource_limits)?;
    let build_metadata = validated.build_metadata.as_ref().map(serde_json::to_value).transpose()?;

    db::insert_version(
        &tx,
        tenant,
        &component.id,
        &version_id,
        &version,
        &object_key,
        &validated.sha256,
        validated.size_bytes as i64,
        &capabilities_json,
        &limits_json,
        build_metadata.as_ref(),
    )
    .await
    // 同一 (component_id, version) 上書きは禁止 (§6.7 MUST NOT) → 409/422。
    .map_err(|e| map_unique_violation(e, "version already exists for this component"))?;

    environment
        .save(&tx, tenant, &component.id, &version_id)
        .await?;
    if let Some(enabled) = ingress {
        db::set_component_ingress(&tx, tenant, &component.id, enabled).await?;
    }
    db::insert_audit_log(&tx, tenant, principal.actor(), "version_environment_published", Some(&component.id),
        Some(&json!({"version_id": version_id, "vars": environment.vars.keys().collect::<Vec<_>>(), "secrets": environment.secrets}))).await?;
    if activate {
        db::switch_active_version(&tx, tenant, &component.id, &version_id).await?;
    }

    let published = components::Entity::find_by_id(component.id.clone()).one(&tx).await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let public_url = if published.ingress_enabled && published.active_version_id.is_some() {
        state.app_url(&component.name, &tenant_info.slug)
    } else { None };
    tx.commit().await?;

    tracing::info!(
        component = %component.name,
        %version,
        sha256 = %validated.sha256,
        size_bytes = validated.size_bytes,
        activate,
        "version uploaded"
    );

    Ok((
        StatusCode::CREATED,
        Json(UploadVersionResponse {
            public_url,
            version_id,
            version,
        }),
    ))
    }.await;
    reservation.finish().await;
    result.map(IntoResponse::into_response)
}

// ---------------------------------------------------------------------------
// GET /components — Component 一覧 (§6.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ComponentListItemResponse {
    pub public_url: Option<String>,
    pub active_version: Option<String>,
    pub active_version_created_at: Option<String>,
    pub component_id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub ingress_enabled: bool,
    pub created_at: String,
}

/// GET /components — テナントの Component 一覧 (deleted_at IS NULL, §6.7)。
pub async fn list_components(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let items = db::list_components(&tx, tenant).await?;
    let tenant_info = db::session_tenant(&tx, tenant)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    tx.commit().await?;
    let body: Vec<ComponentListItemResponse> = items
        .into_iter()
        .map(|c| ComponentListItemResponse {
            public_url: if c.ingress_enabled && c.active_version_id.is_some() {
                state.app_url(&c.name, &tenant_info.0)
            } else {
                None
            },
            active_version: c.active_version,
            active_version_created_at: c
                .active_version_created_at
                .map(|created| created.to_rfc3339()),
            component_id: c.component_id,
            name: c.name,
            active_version_id: c.active_version_id,
            ingress_enabled: c.ingress_enabled,
            created_at: c.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(body))
}

// ---------------------------------------------------------------------------
// GET /components/{id}/versions — version 一覧 (§6.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct VersionListItemResponse {
    pub version_id: String,
    pub version: String,
    pub size_bytes: i64,
    pub wasm_sha256: String,
    pub created_at: String,
    /// Snapshot for the console; DELETE rechecks protection under the component lock.
    pub deletion_blocked_reason: Option<&'static str>,
}

/// GET /components/{id}/versions — 当該 component の version 一覧 (deleted_at IS NULL, §6.7)。
/// component が無ければ 404。
pub async fn list_versions(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // component の存在確認（不在は 404）。
    let component = db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    let items = db::list_versions(&tx, tenant, &component_id).await?;
    let executing: std::collections::HashSet<String> =
        db::active_execution_version_ids(&tx, tenant, &component_id)
            .await?
            .into_iter()
            .collect();
    tx.commit().await?;
    let body: Vec<VersionListItemResponse> = items
        .into_iter()
        .map(|v| VersionListItemResponse {
            deletion_blocked_reason: if component.active_version_id.as_deref()
                == Some(v.version_id.as_str())
            {
                Some("active_version")
            } else if component.previous_active_version_id.as_deref() == Some(v.version_id.as_str())
            {
                Some("rollback_target")
            } else if executing.contains(&v.version_id) {
                Some("active_executions")
            } else {
                None
            },
            version_id: v.version_id,
            version: v.version,
            size_bytes: v.size_bytes,
            wasm_sha256: v.wasm_sha256,
            created_at: v.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(body))
}

// ---------------------------------------------------------------------------
// DELETE /components/{id} — component の soft delete (§6.7)
// ---------------------------------------------------------------------------

/// DELETE /components/{id} — component を soft delete する (§6.7)。
///
/// 保護: 当該 component を参照する pending/running の execution があれば 409 Conflict。
pub async fn delete_component(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    delete_for_tenant(
        &state,
        &principal.tenant_id,
        &component_id,
        principal.actor(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_for_tenant(
    state: &AppState,
    tenant: &str,
    component_id: &str,
    actor: Option<&str>,
) -> Result<(), AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // Serialize deletion with HTTP admission so a new execution cannot slip
    // between the active-execution check and the tombstone update.
    if !db::lock_component(&tx, tenant, component_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }

    // 参照中の実行があれば保護（§6.7）。
    if db::has_active_executions_for_component(&tx, tenant, component_id).await? {
        return Err(FaasError::Conflict(format!(
            "component '{component_id}' has pending/running executions"
        ))
        .into());
    }

    db::soft_delete_component(&tx, tenant, component_id).await?;
    db::insert_audit_log(
        &tx,
        tenant,
        actor,
        "component_deleted",
        Some(component_id),
        None,
    )
    .await?;

    tx.commit().await?;

    tracing::info!(component_id = %component_id, "component soft-deleted");
    Ok(())
}

/// Platform-wide inventory includes suspended test tenants, without bypassing RLS.
pub async fn admin_list_components(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<Value>>, AppError> {
    super::tenants::require_bootstrap_admin(&headers, &state)?;
    let mut result = Vec::new();
    for (tenant_id, tenant_slug) in db::list_tenants_for_admin(state.pool()).await? {
        let tx = state.pool().begin().await?;
        db::set_tenant_guc(&tx, &tenant_id).await?;
        for item in db::list_components(&tx, &tenant_id).await? {
            result.push(json!({"tenant_id": tenant_id, "tenant_slug": tenant_slug,
                "component_id": item.component_id, "name": item.name,
                "active_version_id": item.active_version_id}));
        }
        tx.commit().await?;
    }
    Ok(Json(result))
}

pub async fn admin_delete_component(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((tenant_id, component_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    super::tenants::require_bootstrap_admin(&headers, &state)?;
    delete_for_tenant(&state, &tenant_id, &component_id, Some("bootstrap")).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// DELETE /components/{id}/versions/{version} — version の soft delete (§6.7)
// ---------------------------------------------------------------------------

/// DELETE /components/{id}/versions/{version} — version を soft delete する (§6.7)。
///
/// 保護:
/// - active version（components.active_version_id と一致）は 409（先に active-version を切替える）。
/// - 当該 version を参照する pending/running execution があれば 409。
pub async fn delete_version(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    delete_version_for(
        &state,
        &principal,
        &component_id,
        db::VersionRef::Name(&version),
    )
    .await
}

pub async fn delete_version_by_id(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, version_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    delete_version_for(
        &state,
        &principal,
        &component_id,
        db::VersionRef::Id(&version_id),
    )
    .await
}

async fn delete_version_for(
    state: &AppState,
    principal: &Principal,
    component_id: &str,
    version: db::VersionRef<'_>,
) -> Result<StatusCode, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // Hold the same parent lock as publication, rollback and HTTP admission.
    // Read active/previous/execution state only after any earlier publisher commits.
    if !db::lock_component(&tx, tenant, component_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }
    let component = db::find_component_by_id(&tx, tenant, component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 対象 version の解決（不在は 404）。
    let target_version_id = db::resolve_version_id(&tx, tenant, component_id, version)
        .await?
        .ok_or_else(|| FaasError::NotFound("version".into()))?;

    // active version は削除不可（§6.7: 先に active-version を切替えること）。
    if component.active_version_id.as_deref() == Some(target_version_id.as_str()) {
        return Err(FaasError::Conflict(
            "version is the active version; switch active-version first".into(),
        )
        .into());
    }

    // M7a: rollback の戻り先も削除不可。消すと「ワンクリック rollback が 409 で失敗する」状態に
    // なるため、削除より先に active-version の切替 / rollback を済ませてもらう。
    if component.previous_active_version_id.as_deref() == Some(target_version_id.as_str()) {
        return Err(FaasError::Conflict(
            "version is the rollback target; switch active-version or roll back first".into(),
        )
        .into());
    }

    // 参照中の実行があれば保護（§6.7）。
    if db::has_active_executions_for_version(&tx, tenant, &target_version_id).await? {
        return Err(FaasError::Conflict("version has pending/running executions".into()).into());
    }

    db::soft_delete_version(&tx, tenant, component_id, &target_version_id).await?;
    db::insert_audit_log(
        &tx,
        tenant,
        principal.actor(),
        "version_deleted",
        Some(&target_version_id),
        Some(&json!({"component_id": component_id})),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, version_id = %target_version_id, "version soft-deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// PUT /components/{id}/active-version — active version 切替 (ロールバック, §6.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetActiveVersionRequest {
    /// active 化する既存のバージョン名（旧形式の名前も指定可）。
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct SetActiveVersionResponse {
    pub component_id: String,
    pub active_version_id: String,
    pub version: String,
}

/// PUT /components/{id}/active-version — active_version_id を切替える (ロールバック用, §6.7)。
///
/// 指定 version が当該 component の未削除 version でなければ 404。
pub async fn set_active_version(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    JsonBody(req): JsonBody<SetActiveVersionRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    if req.version.trim().is_empty() {
        return Err(FaasError::InvalidRequest("version must not be empty".into()).into());
    }

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // component の存在確認（不在は 404）。
    db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 指定 version が当該 component の未削除 version であること（不在は 404）。
    let target_version_id = db::find_version_id(&tx, tenant, &component_id, &req.version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!(
                "version '{}' of component '{component_id}'",
                req.version
            ))
        })?;

    tx.commit().await?;
    let reservation =
        crate::preparation::prepare_version(&state, tenant, &target_version_id).await?;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    reservation.lock(&tx).await?;

    if !db::lock_component(&tx, tenant, &component_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }

    if !db::switch_active_version(&tx, tenant, &component_id, &target_version_id).await? {
        return Err(FaasError::NotFound(format!(
            "version '{}' of component '{component_id}'",
            req.version
        ))
        .into());
    }

    // M7a: 版の切替は監査に残す（現行は tracing のみで audit_logs に痕跡が無かった）。
    db::insert_audit_log(
        &tx,
        tenant,
        principal.actor(),
        "active_version_switched",
        Some(&component_id),
        Some(&json!({
            "active_version_id": target_version_id,
            "version": req.version,
        })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(
        component_id = %component_id,
        version = %req.version,
        active_version_id = %target_version_id,
        "active version switched"
    );
    reservation.finish().await;

    Ok(Json(SetActiveVersionResponse {
        component_id,
        active_version_id: target_version_id,
        version: req.version,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RollbackVersionRequest {
    /// 戻り先 (semver)。省略時は `previous_active_version_id`（＝ ワンクリック）。
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RollbackVersionResponse {
    pub component_id: String,
    pub active_version_id: String,
    pub version: String,
    /// 直前まで stable だった版（今回の rollback で置き換えられた側）。
    pub rolled_back_from: Option<String>,
}

/// M11 (§4.2): 公開 HTTP ingress の opt-in を切り替えるリクエスト。
#[derive(Debug, Deserialize)]
pub struct SetIngressRequest {
    /// true で `<app>.<tenant>.<base>` の公開 URL から到達可能にする（既定 false = deny-by-default）。
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct SetIngressResponse {
    pub component_id: String,
    pub ingress_enabled: bool,
}

/// PUT /components/{id}/ingress — 公開 HTTP ingress の opt-in を切り替える (M11, §4.2)。
///
/// Deploy スコープ（component ライフサイクル相当）。deny-by-default なので、公開 URL から
/// 到達させたい component は明示的にこれを true にする必要がある。egress とは無関係。
pub async fn set_component_ingress(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    JsonBody(req): JsonBody<SetIngressRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    let updated = db::set_component_ingress(&tx, tenant, &component_id, req.enabled).await?;
    if !updated {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }
    tx.commit().await?;
    Ok(Json(SetIngressResponse {
        component_id,
        ingress_enabled: req.enabled,
    }))
}

pub async fn rollback_version(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    JsonBody(req): JsonBody<RollbackVersionRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    let target_version_id: Option<String> = match req.version.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => Some(
            db::find_version_id(&tx, tenant, &component_id, v)
                .await?
                .ok_or_else(|| {
                    FaasError::NotFound(format!("version '{v}' of component '{component_id}'"))
                })?,
        ),
        _ => None,
    };

    let prepared_target = match target_version_id.as_ref() {
        Some(id) => id.clone(),
        None => db::find_component_by_id(&tx, tenant, &component_id)
            .await?
            .ok_or_else(|| FaasError::NotFound("component".into()))?
            .previous_active_version_id
            .ok_or_else(|| {
                FaasError::Conflict("no previous version to roll back to; specify a version".into())
            })?,
    };
    tx.commit().await?;
    let reservation = crate::preparation::prepare_version(&state, tenant, &prepared_target).await?;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    reservation.lock(&tx).await?;
    // An implicit rollback must not silently choose a different, unprepared target
    // if another publication completed while preparation was in flight.
    let current_previous: Option<Option<String>> = components::Entity::find()
        .select_only()
        .column(components::Column::PreviousActiveVersionId)
        .filter(components::Column::TenantId.eq(tenant))
        .filter(components::Column::Id.eq(&component_id))
        .filter(components::Column::DeletedAt.is_null())
        .lock_exclusive()
        .into_tuple()
        .one(&tx)
        .await?;
    if target_version_id.is_none()
        && current_previous.flatten().as_deref() != Some(prepared_target.as_str())
    {
        return Err(FaasError::Conflict(
            "rollback target changed during preparation; retry".into(),
        )
        .into());
    }
    let rolled =
        db::rollback_active_version(&tx, tenant, &component_id, Some(&prepared_target)).await?;

    // 0 行のときだけ理由を確定する。
    let Some((active_version_id, rolled_back_from)) = rolled else {
        let component = db::find_component_by_id(&tx, tenant, &component_id)
            .await?
            .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;
        return match (target_version_id, component.previous_active_version_id) {
            // 戻り先を指定していないのに previous が無い（まだ一度も切り替えていない）。
            (None, None) => Err(FaasError::Conflict(
                "no previous version to roll back to; specify a version".into(),
            )
            .into()),
            // previous はあるが解決できない ＝ soft delete 済み（tombstone を active にしない）。
            _ => Err(FaasError::Conflict(
                "rollback target was deleted; specify a live version explicitly".into(),
            )
            .into()),
        };
    };

    let version: String = component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Version)
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::Id.eq(&active_version_id))
        .into_tuple()
        .one(&tx)
        .await?
        .ok_or_else(|| FaasError::NotFound("rollback version".into()))?;

    db::insert_audit_log(
        &tx,
        tenant,
        principal.actor(),
        "version_rollback",
        Some(&component_id),
        Some(&json!({
            "active_version_id": active_version_id,
            "rolled_back_from": rolled_back_from,
        })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, %active_version_id, "rolled back");
    reservation.finish().await;

    Ok(Json(RollbackVersionResponse {
        component_id,
        active_version_id,
        version,
        rolled_back_from,
    }))
}
