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
use hibana_shared::{new_component_id, new_version_id, FaasError, ResourceLimits};
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

    if req.name.trim().is_empty() {
        return Err(FaasError::InvalidRequest("name must not be empty".into()).into());
    }

    let component_id = new_component_id();

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    db::create_component(&tx, tenant, &component_id, &req.name)
        .await
        .map_err(|e| map_unique_violation(e, "component name already exists"))?;
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
    pub status: &'static str,
}

/// POST /components/{component_id}/versions — アップロード+検証+デプロイ (§6.2)。
///
/// multipart フィールド:
/// - `version` (必須): semver
/// - `wasm` (必須): wasm component バイナリ
/// - `capabilities` (任意): JSON。M2 は受理して検証結果と整合確認するのみ（列保存は §4.4）
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
/// 5. 検証通過まで status=pending → 通過後に MinIO 保存 → INSERT(active) → active 化
pub async fn upload_version(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // 対象 component の存在確認（active 化先・名前解決のため）。
    let component = db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    tx.commit().await?; // No DB transaction while receiving or validating the upload.

    // --- multipart フィールドを収集 ---
    let mut version: Option<String> = None;
    let mut wasm_bytes: Option<Vec<u8>> = None;
    let mut capabilities: Option<Value> = None;
    // M9a: 本体 sha256 に対する detached Ed25519 署名（base64url）。任意フィールド。
    let mut signature: Option<String> = None;
    let mut resource_limits: ResourceLimits = ResourceLimits::default();
    let mut activate = true;
    let mut ingress: Option<bool> = None;
    let mut environment = super::deployment::VersionEnvironment::default();
    let mut fields = std::collections::BTreeSet::new();

    let max_bytes = state.max_wasm_upload_bytes();

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| FaasError::InvalidRequest(format!("malformed multipart: {e}")))?
    {
        if let Some(name) = field.name() {
            if !fields.insert(name.to_owned()) {
                return Err(FaasError::InvalidRequest("duplicate multipart field".into()).into());
            }
        }
        match field.name() {
            Some("version") => {
                let v = field.text().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("invalid version field: {e}"))
                })?;
                version = Some(v);
            }
            Some("capabilities") => {
                let text = field.text().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("invalid capabilities field: {e}"))
                })?;
                let parsed: Value = serde_json::from_str(&text).map_err(|e| {
                    FaasError::InvalidRequest(format!("capabilities is not valid JSON: {e}"))
                })?;
                // envはvarsと承認済みSecret参照から構築する。生のcapabilities.envは拒否。
                validation::reject_env_in_declared_capabilities(&parsed)?;
                capabilities = Some(parsed);
            }
            Some("resource_limits") => {
                let text = field.text().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("invalid resource_limits field: {e}"))
                })?;
                resource_limits = serde_json::from_str(&text).map_err(|e| {
                    FaasError::InvalidRequest(format!("resource_limits is not valid JSON: {e}"))
                })?;
                // M4b (§4.3): §4.3 表の上限を超える値（max_memory > 1 GiB / max_wall_time > 30s /
                // max_execution_time > 60s）や論理不整合（max_execution_time < max_wall_time、
                // 各ゼロ値、max_fuel = Some(0)）はここで 422 で拒否する。worker 側の防御もあるが、
                // 受付時点で明示的に弾くことで「保存だけされて毎 invoke で落ちる」を防ぐ。
                resource_limits.validate()?;
            }
            Some("wasm") => {
                // (1) ストリーミング中にサイズ上限を強制。超過は即打ち切り。
                let mut buf: Vec<u8> = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("error reading wasm field: {e}"))
                })? {
                    if buf.len() as u64 + chunk.len() as u64 > max_bytes {
                        return Err(FaasError::InvalidRequest(format!(
                            "wasm exceeds max upload size of {max_bytes} bytes"
                        ))
                        .into());
                    }
                    buf.extend_from_slice(&chunk);
                }
                wasm_bytes = Some(buf);
            }
            Some("activate" | "ingress" | "vars" | "secrets") => {
                let name = field.name().unwrap().to_owned();
                let text = field
                    .text()
                    .await
                    .map_err(|_| FaasError::InvalidRequest("invalid deployment field".into()))?;
                // Do not include JSON parser errors: they can quote user-supplied values.
                let invalid = || FaasError::InvalidRequest(format!("invalid {name} field"));
                match name.as_str() {
                    "activate" => activate = serde_json::from_str(&text).map_err(|_| invalid())?,
                    "ingress" => {
                        ingress = Some(serde_json::from_str(&text).map_err(|_| invalid())?)
                    }
                    "vars" => {
                        environment.vars = serde_json::from_str(&text).map_err(|_| invalid())?
                    }
                    "secrets" => {
                        environment.secrets = serde_json::from_str(&text).map_err(|_| invalid())?
                    }
                    _ => unreachable!(),
                }
            }
            Some("signature") => {
                // M9a: 本体 sha256 に対する detached Ed25519 署名（base64url）。
                let v = field.text().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("invalid signature field: {e}"))
                })?;
                let v = v.trim().to_string();
                if !v.is_empty() {
                    signature = Some(v);
                }
            }
            // 未知フィールドは無視（前方互換）。
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let version = version
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| FaasError::InvalidRequest("missing required field 'version'".into()))?;
    let wasm_bytes = wasm_bytes
        .filter(|b| !b.is_empty())
        .ok_or_else(|| FaasError::InvalidRequest("missing required field 'wasm'".into()))?;

    let allowed_env = environment.validate()?;
    if ingress.is_some() && !activate {
        return Err(FaasError::InvalidRequest("ingress requires activate=true".into()).into());
    }

    // (2)(3)(4) 隔離検証 + §4.4 capability strict matching + sha256/サイズ確定。
    //
    // Persist imports validated against Hibana's WASI host contract. Client
    // declarations cannot add host functions or grant runtime permissions.
    let validated = validation::validate_wasm(wasm_bytes.clone()).await?;

    if let Some(caps) = &capabilities {
        // 宣言値は監査・観測用にログするのみ（保存・付与には使わない, §4.4）。
        tracing::debug!(declared = ?caps, imports = ?validated.imports, "client-declared capabilities (not trusted; persisting resolved approved set)");
    }
    // §4.4 MUST: クライアント宣言値ではなく、承認集合と照合して解決した import を保存する。
    //
    // Plain vars and selected, separately authorized Secrets form the environment.

    // (5) 検証通過 → MinIO 保存 → INSERT(active) → active 化。
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
        let audit_reject = |reason: &'static str| {
            tracing::warn!(%component_id, reason, require_signed, "component signature rejected");
        };
        let sig = signature.as_deref().ok_or_else(|| {
            audit_reject("signature_required");
            FaasError::InvalidRequest(
                "this tenant requires signed components; provide a 'signature' field \
                 (detached Ed25519 over the wasm sha256, base64url)"
                    .into(),
            )
        })?;

        let keys = db::list_signing_keys(&tx, tenant)
            .await?
            .into_iter()
            .map(|k| (k.key_id, k.public_key))
            .collect::<Vec<_>>();
        if keys.is_empty() {
            audit_reject("no_signing_keys_registered");
            return Err(FaasError::InvalidRequest(
                "component signature present/required but no signing keys are registered for this \
                 tenant (register one via PUT /admin/signing-keys/{key_id})"
                    .into(),
            )
            .into());
        }

        if let Err(e) = signing::verify_component_signature(&validated.sha256, sig, &keys) {
            audit_reject(e.reason());
            db::insert_audit_log(
                &tx,
                tenant,
                principal.user_id.as_deref(),
                "component_signature_rejected",
                Some(&component.id),
                Some(&json!({
                    "version": version,
                    "sha256": validated.sha256,
                    "reason": e.reason(),
                })),
            )
            .await?;
            return Err(FaasError::InvalidRequest(format!(
                "component signature verification failed: {}",
                e.reason()
            ))
            .into());
        }
    }

    // Read after the parent lock: deployments cannot race an administrator's
    // approval/revocation or grant permissions supplied by the uploaded artifact.
    let current = db::find_component_by_id(&tx, tenant, &component_id).await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let capabilities_json = hibana_shared::capabilities::CapabilitySet {
        imports: validated.approved_imports.clone(),
        env: allowed_env,
        net_allow_outbound: super::egress::policy(&current)?.unwrap_or_default(),
    }.to_json();

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
        "active",
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
    db::insert_audit_log(&tx, tenant, principal.user_id.as_deref(), "version_environment_published", Some(&component.id),
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
            status: "active",
        }),
    ))
    }.await;
    reservation.finish().await;
    result
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
    let active_ids: Vec<_> = items
        .iter()
        .filter_map(|c| c.active_version_id.clone())
        .collect();
    let active: std::collections::HashMap<_, _> = component_versions::Entity::find()
        .select_only()
        .columns([
            component_versions::Column::Id,
            component_versions::Column::Version,
            component_versions::Column::CreatedAt,
        ])
        .filter(component_versions::Column::TenantId.eq(tenant))
        .filter(component_versions::Column::Id.is_in(active_ids))
        .filter(component_versions::Column::DeletedAt.is_null())
        .into_tuple::<(String, String, chrono::DateTime<chrono::Utc>)>()
        .all(&tx)
        .await?
        .into_iter()
        .map(|(id, version, created)| (id, (version, created)))
        .collect();
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
            active_version: c
                .active_version_id
                .as_ref()
                .and_then(|id| active.get(id))
                .map(|v| v.0.clone()),
            active_version_created_at: c
                .active_version_id
                .as_ref()
                .and_then(|id| active.get(id))
                .map(|v| v.1.to_rfc3339()),
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
    pub status: String,
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
            status: v.status,
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
    delete_for_tenant(&state, &principal.tenant_id, &component_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_for_tenant(
    state: &AppState,
    tenant: &str,
    component_id: &str,
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
    delete_for_tenant(&state, &tenant_id, &component_id).await?;
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
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;

    // Hold the same parent lock as publication, rollback and HTTP admission.
    // Read active/previous/execution state only after any earlier publisher commits.
    if !db::lock_component(&tx, tenant, &component_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }
    let component = db::find_component_by_id(&tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 対象 version の解決（不在は 404）。
    let target_version_id = db::find_version_id(&tx, tenant, &component_id, &version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!("version '{version}' of component '{component_id}'"))
        })?;

    // active version は削除不可（§6.7: 先に active-version を切替えること）。
    if component.active_version_id.as_deref() == Some(target_version_id.as_str()) {
        return Err(FaasError::Conflict(format!(
            "version '{version}' is the active version; switch active-version first"
        ))
        .into());
    }

    // M7a: rollback の戻り先も削除不可。消すと「ワンクリック rollback が 409 で失敗する」状態に
    // なるため、削除より先に active-version の切替 / rollback を済ませてもらう。
    if component.previous_active_version_id.as_deref() == Some(target_version_id.as_str()) {
        return Err(FaasError::Conflict(format!(
            "version '{version}' is the rollback target; switch active-version or roll back first"
        ))
        .into());
    }

    // 参照中の実行があれば保護（§6.7）。
    if db::has_active_executions_for_version(&tx, tenant, &target_version_id).await? {
        return Err(FaasError::Conflict(format!(
            "version '{version}' has pending/running executions"
        ))
        .into());
    }

    db::soft_delete_version(&tx, tenant, &component_id, &target_version_id).await?;

    tx.commit().await?;

    tracing::info!(component_id = %component_id, %version, "version soft-deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// PUT /components/{id}/active-version — active version 切替 (ロールバック, §6.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetActiveVersionRequest {
    /// active 化する既存の version (semver)。
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
        principal.user_id.as_deref(),
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

    db::insert_audit_log(
        &tx,
        tenant,
        principal.user_id.as_deref(),
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
        rolled_back_from,
    }))
}
