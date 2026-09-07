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
use faas_shared::{
    component_object_key, new_component_id, new_version_id, FaasError, ResourceLimits,
};
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

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    db::create_component(&mut *tx, tenant, &component_id, &req.name)
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
/// - `resource_limits` (任意): JSON (faas_shared::ResourceLimits)
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

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // 対象 component の存在確認（active 化先・名前解決のため）。
    let component = db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // --- multipart フィールドを収集 ---
    let mut version: Option<String> = None;
    let mut wasm_bytes: Option<Vec<u8>> = None;
    let mut capabilities: Option<Value> = None;
    // M9a: 本体 sha256 に対する detached Ed25519 署名（base64url）。任意フィールド。
    let mut signature: Option<String> = None;
    let mut resource_limits: ResourceLimits = ResourceLimits::default();
    let mut activate = true;

    let max_bytes = state.max_wasm_upload_bytes();

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| FaasError::InvalidRequest(format!("malformed multipart: {e}")))?
    {
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
                // M7b (§4.4): env 許可リストは admin 承認の対象。deploy スコープのこの経路で
                // 宣言されたら 400 で拒否する（黙って無視すると「設定したのに効かない」になる）。
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
            Some("activate") => {
                let text = field.text().await.map_err(|e| {
                    FaasError::InvalidRequest(format!("invalid activate field: {e}"))
                })?;
                // `"false"` だけを false と解釈する（未知値は既定 true ＝ 従来挙動へ倒す）。
                activate = !text.trim().eq_ignore_ascii_case("false");
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

    // (2)(3)(4) 隔離検証 + §4.4 capability strict matching + sha256/サイズ確定。
    //
    // §4.4 (M3d): capability は既定 **deny-all**。クライアント宣言値（multipart `capabilities`）は
    // **信用しない**（承認は admin スコープの管理操作）。validate_wasm は WIT import を
    // **admin 承認集合**（本スライスは標準 handler world 契約 + 標準 WASI の baseline）と厳密照合し、
    // 未承認 import は 422 で拒否する。クライアント宣言値は受理してログに残すだけで、保存は
    // **承認済みとして解決した import 集合**（validated.approved_imports）を使う。
    let approved = validation::ApprovedCapabilities::baseline();
    let validated = validation::validate_wasm(wasm_bytes.clone(), approved).await?;

    // M9a (§6.2): 供給網検証 —— テナントが登録した公開鍵での署名検証。
    //
    // deploy トークンは「アップロードの認可」、署名は「本体の真正性」で、**別々の秘密**に
    // 依存させる（片方が漏れても攻撃が成立しない）。ポリシー `require_signed_components`:
    //   - true: 署名が必須。無い / 検証失敗 / 鍵不在は 422（fail-closed）。
    //   - false（既定）: 署名が**有れば**検証する（不正署名は拒否）。無ければ従来どおり通す。
    // どちらでも「署名が付いていて不正」は拒否する（誤検証を黙認しない）。
    //
    // 署名対象は本体そのものではなく `validated.sha256`（検証パイプラインが算出済みの 16 進文字列）。
    let require_signed = db::require_signed_components(&mut *tx, tenant).await?;
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

        let keys = db::list_signing_keys(&mut *tx, tenant)
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
                &mut *tx,
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

    if let Some(caps) = &capabilities {
        // 宣言値は監査・観測用にログするのみ（保存・付与には使わない, §4.4）。
        tracing::debug!(declared = ?caps, imports = ?validated.imports, "client-declared capabilities (not trusted; persisting resolved approved set)");
    }
    // §4.4 MUST: クライアント宣言値ではなく、承認集合と照合して解決した import を保存する。
    //
    // M7b: `capabilities.env`（注入を許可する env 名の許可リスト）は **admin 承認**の対象であり、
    // Deploy スコープのこの経路では**常に空**（deny-all）で保存する。deploy トークンが
    // `env: ["PROD_API_KEY"]` を宣言できると、その wasm が `wasi:cli/environment`（baseline 承認済み）
    // で読んだ値を invoke 出力へ返すだけで admin 専用の secret を平文で取得できる（権限昇格）。
    // M9c: egress allowlist も upload（deploy スコープ）では**常に空**（deny-all）で保存する。
    // `wasi:sockets/*` の import は baseline 承認だが、実際の外部到達は admin が
    // `PUT .../capabilities/egress` で承認するまで worker の socket_addr_check が全拒否する。
    let capabilities_json = validation::CapabilitySet {
        imports: validated.approved_imports.clone(),
        env: std::collections::BTreeSet::new(),
        net_allow_outbound: std::collections::BTreeSet::new(),
    }
    .to_json();

    // (5) 検証通過 → MinIO 保存 → INSERT(active) → active 化。
    let object_key = component_object_key(tenant, &component.name, &version);
    state
        .storage()
        .put_object(&object_key, wasm_bytes, "application/wasm")
        .await?;

    let version_id = new_version_id();
    let limits_json = serde_json::to_value(resource_limits)?;

    db::insert_version(
        &mut *tx,
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
    )
    .await
    // 同一 (component_id, version) 上書きは禁止 (§6.7 MUST NOT) → 409/422。
    .map_err(|e| map_unique_violation(e, "version already exists for this component"))?;

    if activate {
        db::switch_active_version(&mut *tx, tenant, &component.id, &version_id).await?;
    }

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
            version_id,
            version,
            status: "active",
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /components — Component 一覧 (§6.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ComponentListItemResponse {
    pub component_id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub created_at: String,
}

/// GET /components — テナントの Component 一覧 (deleted_at IS NULL, §6.7)。
pub async fn list_components(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let items = db::list_components(&mut *tx, tenant).await?;
    tx.commit().await?;
    let body: Vec<ComponentListItemResponse> = items
        .into_iter()
        .map(|c| ComponentListItemResponse {
            component_id: c.component_id,
            name: c.name,
            active_version_id: c.active_version_id,
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
}

/// GET /components/{id}/versions — 当該 component の version 一覧 (deleted_at IS NULL, §6.7)。
/// component が無ければ 404。
pub async fn list_versions(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // component の存在確認（不在は 404）。
    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    let items = db::list_versions(&mut *tx, tenant, &component_id).await?;
    tx.commit().await?;
    let body: Vec<VersionListItemResponse> = items
        .into_iter()
        .map(|v| VersionListItemResponse {
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
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // 存在確認（不在は 404）。
    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 参照中の実行があれば保護（§6.7）。
    if db::has_active_executions_for_component(&mut *tx, tenant, &component_id).await? {
        return Err(FaasError::Conflict(format!(
            "component '{component_id}' has pending/running executions"
        ))
        .into());
    }

    db::soft_delete_component(&mut *tx, tenant, &component_id).await?;

    tx.commit().await?;

    tracing::info!(component_id = %component_id, "component soft-deleted");
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

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // component の存在確認（不在は 404）。active_version_id も引く。
    let component = db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 対象 version の解決（不在は 404）。
    let target_version_id = db::find_version_id(&mut *tx, tenant, &component_id, &version)
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
    if db::has_active_executions_for_version(&mut *tx, tenant, &target_version_id).await? {
        return Err(FaasError::Conflict(format!(
            "version '{version}' has pending/running executions"
        ))
        .into());
    }

    db::soft_delete_version(&mut *tx, tenant, &component_id, &target_version_id).await?;

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

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // component の存在確認（不在は 404）。
    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    // 指定 version が当該 component の未削除 version であること（不在は 404）。
    let target_version_id = db::find_version_id(&mut *tx, tenant, &component_id, &req.version)
        .await?
        .ok_or_else(|| {
            FaasError::NotFound(format!(
                "version '{}' of component '{component_id}'",
                req.version
            ))
        })?;

    if !db::switch_active_version(&mut *tx, tenant, &component_id, &target_version_id).await? {
        return Err(FaasError::NotFound(format!("component '{component_id}'")).into());
    }

    // M7a: 版の切替は監査に残す（現行は tracing のみで audit_logs に痕跡が無かった）。
    db::insert_audit_log(
        &mut *tx,
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
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let updated = db::set_component_ingress(&mut *tx, tenant, &component_id, req.enabled).await?;
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

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    let target_version_id: Option<String> = match req.version.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => Some(
            db::find_version_id(&mut *tx, tenant, &component_id, v)
                .await?
                .ok_or_else(|| {
                    FaasError::NotFound(format!("version '{v}' of component '{component_id}'"))
                })?,
        ),
        _ => None,
    };

    let rolled = db::rollback_active_version(
        &mut *tx,
        tenant,
        &component_id,
        target_version_id.as_deref(),
    )
    .await?;

    // 0 行のときだけ理由を確定する。
    let Some((active_version_id, rolled_back_from)) = rolled else {
        let component = db::find_component_by_id(&mut *tx, tenant, &component_id)
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
        &mut *tx,
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

    Ok(Json(RollbackVersionResponse {
        component_id,
        active_version_id,
        rolled_back_from,
    }))
}
