//! Configuration management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use faas_shared::FaasError;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ---------------------------------------------------------------------------
// M7b: per-function 環境変数（平文 config）の CRUD（§15 / §4.4）
//
// スコープは **deploy**（読み書きとも）。読み取りを read に置かない理由: config は平文であり、
// 注入時に secret と同じ env 名前空間へ混ざる。運用者が資格情報を誤って config 側に入れる確率は
// 現実的に高く、その瞬間 read スコープ（監視・ダッシュボード用途で最も広く配られる）が
// 資格情報の読み取り権限になってしまう。1 段引き上げて被害面を縮める。
// → README の露出ガード節に「config は平文であり deploy スコープで読める。資格情報は必ず
//    secrets 側に置くこと」を明記する。
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PutFunctionConfigRequest {
    /// 環境変数の**全置換**マップ。空オブジェクトで全削除。
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct FunctionConfigResponse {
    pub component_id: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// config の入力（キー名・値長・件数・総バイト）を検証する純関数（DB / ストア非依存）。
///
/// 上限は `faas_shared` の定数を使う（CP の受付と worker の防御的 clamp で同じ値を参照する
/// 二重防御）。超過は 400（`error.rs` は `InvalidRequest` を 400 にしか写像しない）。
pub(super) fn validate_env_map(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<(), FaasError> {
    if env.len() > faas_shared::MAX_FUNCTION_ENV_KEYS {
        return Err(FaasError::InvalidRequest(format!(
            "at most {} env entries are allowed per component",
            faas_shared::MAX_FUNCTION_ENV_KEYS
        )));
    }
    let mut total = 0usize;
    for (k, v) in env {
        if !faas_shared::is_valid_env_key(k) {
            return Err(FaasError::InvalidRequest(format!(
                "invalid env key '{k}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
                faas_shared::MAX_ENV_KEY_LEN - 1
            )));
        }
        if v.len() > faas_shared::MAX_ENV_VALUE_BYTES {
            return Err(FaasError::InvalidRequest(format!(
                "env value for '{k}' exceeds {} bytes",
                faas_shared::MAX_ENV_VALUE_BYTES
            )));
        }
        total += k.len() + v.len();
    }
    if total > faas_shared::MAX_FUNCTION_ENV_TOTAL_BYTES {
        return Err(FaasError::InvalidRequest(format!(
            "total env size exceeds {} bytes",
            faas_shared::MAX_FUNCTION_ENV_TOTAL_BYTES
        )));
    }
    Ok(())
}

/// GET /components/{id}/config — 平文 config を返す（deploy）。
pub async fn get_function_config(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    let rows = db::list_function_configs(&mut *tx, tenant, &component_id).await?;
    tx.commit().await?;

    let updated_at = rows.iter().map(|r| r.updated_at).max();
    Ok(Json(FunctionConfigResponse {
        component_id,
        env: rows.into_iter().map(|r| (r.key, r.value)).collect(),
        updated_at,
    }))
}

/// PUT /components/{id}/config — 平文 config を全置換する（deploy）。
///
/// `PATCH` は既存 API の作法に無いので使わない（全置換のみ）。
pub async fn put_function_config(
    State(state): State<AppState>,
    principal: Principal,
    Path(component_id): Path<String>,
    JsonBody(req): JsonBody<PutFunctionConfigRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;
    validate_env_map(&req.env)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    let entries: Vec<(String, String)> = req
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    db::replace_function_configs(
        &mut tx,
        tenant,
        &component_id,
        &entries,
        principal.user_id.as_deref(),
    )
    .await?;

    // 監査には**キー名だけ**を残す（値は載せない: config は平文だが、誤って資格情報が
    // 入れられていた場合に audit_logs へ二次的に残さないため）。
    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "function_config_updated",
        Some(&component_id),
        Some(&json!({ "keys": req.env.keys().collect::<Vec<_>>() })),
    )
    .await?;

    let rows = db::list_function_configs(&mut *tx, tenant, &component_id).await?;
    tx.commit().await?;

    tracing::info!(%component_id, key_count = rows.len(), "function config replaced");

    let updated_at = rows.iter().map(|r| r.updated_at).max();
    Ok(Json(FunctionConfigResponse {
        component_id,
        env: rows.into_iter().map(|r| (r.key, r.value)).collect(),
        updated_at,
    }))
}

/// DELETE /components/{id}/config/{key} — 1 キー削除（deploy）。不在は 404。
pub async fn delete_function_config(
    State(state): State<AppState>,
    principal: Principal,
    Path((component_id, key)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    db::find_component_by_id(&mut *tx, tenant, &component_id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{component_id}'")))?;

    if !db::delete_function_config(&mut *tx, tenant, &component_id, &key).await? {
        return Err(FaasError::NotFound(format!(
            "config key '{key}' of component '{component_id}'"
        ))
        .into());
    }

    db::insert_audit_log(
        &mut *tx,
        tenant,
        principal.user_id.as_deref(),
        "function_config_deleted",
        Some(&component_id),
        Some(&json!({ "key": key })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(%component_id, %key, "function config key deleted");
    Ok(StatusCode::NO_CONTENT)
}
