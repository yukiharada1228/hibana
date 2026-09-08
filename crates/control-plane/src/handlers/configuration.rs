//! Configuration management HTTP handlers.
use crate::auth::Principal;
use crate::db;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use hibana_shared::FaasError;
use serde::Serialize;

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

#[derive(Debug, Serialize)]
pub struct FunctionConfigResponse {
    pub component_id: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// config の入力（キー名・値長・件数・総バイト）を検証する純関数（DB / ストア非依存）。
///
/// 上限は `hibana_shared` の定数を使う（CP の受付と worker の防御的 clamp で同じ値を参照する
/// 二重防御）。超過は 400（`error.rs` は `InvalidRequest` を 400 にしか写像しない）。
pub(super) fn validate_env_map(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<(), FaasError> {
    if env.len() > hibana_shared::MAX_FUNCTION_ENV_KEYS {
        return Err(FaasError::InvalidRequest(format!(
            "at most {} env entries are allowed per component",
            hibana_shared::MAX_FUNCTION_ENV_KEYS
        )));
    }
    let mut total = 0usize;
    for (k, v) in env {
        if !hibana_shared::is_valid_env_key(k) {
            return Err(FaasError::InvalidRequest(format!(
                "invalid env key '{k}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
                hibana_shared::MAX_ENV_KEY_LEN - 1
            )));
        }
        if v.len() > hibana_shared::MAX_ENV_VALUE_BYTES {
            return Err(FaasError::InvalidRequest(format!(
                "env value for '{k}' exceeds {} bytes",
                hibana_shared::MAX_ENV_VALUE_BYTES
            )));
        }
        total += k.len() + v.len();
    }
    if total > hibana_shared::MAX_FUNCTION_ENV_TOTAL_BYTES {
        return Err(FaasError::InvalidRequest(format!(
            "total env size exceeds {} bytes",
            hibana_shared::MAX_FUNCTION_ENV_TOTAL_BYTES
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

/// Legacy writes fail explicitly: vars can only be changed by publishing a version.
pub async fn put_function_config() -> Result<(), AppError> {
    Err(FaasError::Conflict(
        "vars are versioned; deploy a new version with the vars multipart field".into(),
    )
    .into())
}

pub async fn delete_function_config() -> Result<(), AppError> {
    put_function_config().await
}
