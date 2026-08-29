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

// ---------------------------------------------------------------------------
// M7c: POST /internal/job-env — worker への secret 引き換え（§4.6）
//
// **このハンドラは内部専用 listener（INTERNAL_BIND_ADDR）にだけマウントする。**
// 公開 listener（BIND_ADDR）に生やしてはならない (MUST NOT)。認証 middleware の外にあるため、
// 認証は env-token の署名そのもの、テナント停止の遮断はこのハンドラ内で明示的に行う。
//
// ## 検証手順（順序も規約, §4.6.2）
//  1. per-IP レート制限（無認証面なのでこれが最初）
//  2. 署名検証（aud == "job-env" を要求。job_token を渡しても署名ドメインが違うので通らない）
//  3. exp 検査
//  4. tenants.status = 'active'（middleware 外なので明示。MUST）
//  5. set_tenant_guc した tx を開始（以降すべて RLS 下）
//  6. executions 行と claim の突き合わせ（tenant / version / component 一致、未終端であること）
//  7. テナント名前空間のレート制限（invoke 予算を食わない独立名前空間）
//  8. capabilities.env の許可リストを解決（パース不能なら deny-all）
//  9. 許可リストで絞って secret のみ復号（**execution 基準の世代**）
// 10. {"env": {...}} を返す
// 11. 監査（値は載せない）
// ---------------------------------------------------------------------------

/// 引き換えリクエスト。**`Debug` を derive しない**（トークンをログに出さない）。
#[derive(Deserialize)]
pub struct JobEnvRequest {
    pub env_token: String,
}

/// per-IP レート制限のキー名前空間（invoke の `rl:{tenant}` と衝突させない）。
fn env_ip_rate_key(ip: &str) -> String {
    format!("env-ip:{ip}")
}

/// テナント名前空間のレート制限キー（invoke 予算を食わない独立名前空間）。
fn env_tenant_rate_key(tenant: &str) -> String {
    format!("env:{tenant}")
}

/// 引き換え失敗を監査に残す（best-effort。理由は安定文字列のみ、値は載せない）。
async fn audit_denied(state: &AppState, tenant: &str, target: Option<&str>, reason: &str) {
    let mut tx = match state.pool().begin().await {
        Ok(t) => t,
        Err(_) => return,
    };
    if db::set_tenant_guc(&mut tx, tenant).await.is_err() {
        return;
    }
    let _ = db::insert_audit_log(
        &mut *tx,
        tenant,
        None,
        "secret_material_denied",
        target,
        Some(&json!({ "reason": reason })),
    )
    .await;
    let _ = tx.commit().await;
}

/// POST /internal/job-env — env-token と引き換えに復号済み env を返す。
pub async fn job_env(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    JsonBody(req): JsonBody<JobEnvRequest>,
) -> Result<impl IntoResponse, AppError> {
    // --- 1) per-IP レート制限（無認証面のグローバル保護。**最初に**行う） ---
    let ip = peer.ip().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let per_min = state.job_env_exchange_rate_per_min() as f64;
    let ip_params = crate::store::RateLimitParams {
        refill_per_sec: per_min / 60.0,
        capacity: per_min,
    };
    // fail-open（ストア障害時は許可）は invoke と同じ方針。ここは無認証面の粗いガードであり、
    // 実質的な認証は env-token の署名（手順 2）と行の突き合わせ（手順 6）が担う。
    if let Ok(d) = state
        .store()
        .rate_limit(&env_ip_rate_key(&ip), ip_params, now_ms)
        .await
    {
        if !d.allowed {
            return Ok(crate::admission::RateLimited::rate(d.retry_after_secs).into_response());
        }
    }

    // --- 2) 署名検証 + aud 検査 ---
    // job_token を渡しても署名ドメインタグが違うので通らない。aud はさらにその二重確認。
    let claims = state
        .signer()
        .verifier()
        .verify_env(&req.env_token)
        .map_err(|_| FaasError::Unauthorized)?;
    if claims.aud != faas_shared::ENV_TOKEN_AUDIENCE {
        return Err(FaasError::Unauthorized.into());
    }

    // --- 3) exp 検査 ---
    let now = chrono::Utc::now().timestamp();
    if claims.exp <= now {
        return Err(FaasError::Unauthorized.into());
    }

    // --- 4) テナント停止の遮断（middleware 外なので明示。MUST, §4.6.1 (3)） ---
    if !db::tenant_is_active(state.pool(), &claims.tenant_id).await? {
        return Err(FaasError::Forbidden.into());
    }

    // --- 5) 以降すべて RLS 下 ---
    let tenant = claims.tenant_id.clone();
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &tenant).await?;

    // --- 6) executions 行と claim の突き合わせ ---
    // subscriber が result 側で行っている claim ↔ 行の照合と同じ思想。
    let Some(exec) = db::get_execution(&mut *tx, &tenant, &claims.execution_id).await? else {
        drop(tx);
        audit_denied(
            &state,
            &tenant,
            Some(&claims.execution_id),
            "execution_not_found",
        )
        .await;
        return Err(FaasError::Unauthorized.into());
    };
    if exec.version_id != claims.version_id || exec.component_id != claims.component_id {
        drop(tx);
        audit_denied(
            &state,
            &tenant,
            Some(&claims.execution_id),
            "claim_row_mismatch",
        )
        .await;
        return Err(FaasError::Unauthorized.into());
    }
    // 終端済みの execution へは発行しない（再配送の窓を過ぎた要求は拒否する）。
    if !matches!(exec.status.as_str(), "pending" | "running") {
        drop(tx);
        audit_denied(
            &state,
            &tenant,
            Some(&claims.execution_id),
            "execution_terminal",
        )
        .await;
        return Err(FaasError::Forbidden.into());
    }

    // --- 7) テナント名前空間のレート制限（invoke 予算とは独立） ---
    // JetStream の再配送（max_deliver 既定 5）を許容するため single-use にはしない。
    if let Ok(d) = state
        .store()
        .rate_limit(&env_tenant_rate_key(&tenant), ip_params, now_ms)
        .await
    {
        if !d.allowed {
            return Ok(crate::admission::RateLimited::rate(d.retry_after_secs).into_response());
        }
    }

    // --- 8) 許可リスト（admin 承認）を解決。壊れた値は deny-all。 ---
    let capabilities = db::version_capabilities(&mut *tx, &tenant, &claims.version_id)
        .await?
        .unwrap_or(serde_json::Value::Null);
    let allowed = crate::validation::parse_capabilities(&capabilities).env;

    // --- 9) 許可リストで絞って復号（**execution 基準の世代**, §4.7.1） ---
    let resolved = secrets::resolve_for_injection(
        &mut tx,
        state.secret_keyring(),
        &tenant,
        &claims.component_id,
        exec.created_at,
        &allowed,
    )
    .await;

    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => {
            let reason = e.reason();
            drop(tx);
            audit_denied(&state, &tenant, Some(&claims.execution_id), reason).await;
            return Err(map_secret_error(e));
        }
    };

    // --- 11) 監査（**値も件数以外の内訳も載せない**: 名前と件数まで） ---
    let names: Vec<&str> = resolved.iter().map(|r| r.name.as_str()).collect();
    db::insert_audit_log(
        &mut *tx,
        &tenant,
        None,
        "secret_material_issued",
        Some(&claims.execution_id),
        Some(&json!({ "names": names, "count": resolved.len() })),
    )
    .await?;
    tx.commit().await?;

    state
        .metrics()
        .secret_material_issued_total
        .with_label_values(&["ok"])
        .inc();

    // --- 10) 応答。ここが平文の唯一の出口であり、TLS 上の HTTP レスポンスにしか現れない
    //         （NATS にも DB にも S3 にも平文は書かない）。
    let env: std::collections::BTreeMap<String, &str> = resolved
        .iter()
        .map(|r| (r.name.clone(), r.value.expose().as_str()))
        .collect();

    Ok(Json(json!({ "env": env })).into_response())
}

// ---------------------------------------------------------------------------
// M7c-4: KEK ローテーション（wrap-only rekey, §4.7.2）
//
// **これは侵害復旧ではない**（§4.7.2 / secrets.rs の doc）。DEK も ciphertext も不変なので、
// 旧 KEK + 旧 DB ダンプを持つ攻撃者は再ラップ後も全平文を復元できる。
// rekey は **KEK の計画的ローテーション専用**であり、侵害時の唯一の復旧経路は
// 「値そのものを rotate する」ことである（README 露出ガード節に明記）。
//
// 手順:
//   1. SECRETS_MASTER_KID を新 kid に切り替え、旧 kid を SECRETS_RETIRED_KEYS へ移す
//      （この時点で新規書き込みは新 kid、既存は旧 kid で復号可能）。
//   2. この API（または背景ジョブ）が対象を引き、reason='rekey' の新 version を INSERT して
//      current_version を前進させる。
//   3. すべて再ラップし終えてから SECRETS_RETIRED_KEYS を空にする。
//      **早期撤去は復号不能 ＝ データ喪失**（signing の overlap と意味が違う: signing は TTL で
//      有限時間に終わるが、secret の暗号文は DB に永続する）。
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RekeyResponse {
    /// 再ラップした secret の件数。
    ///
    /// **kid 別の内訳は返さない**。本リポジトリの admin はテナント管理者であり、全テナント
    /// 横断の集計を返すと他テナントの secret 総数が漏れる。kid 別の全体像は Prometheus gauge
    /// （内部更新）でのみ観測する。
    pub rewrapped: usize,
}

/// POST /admin/secrets/rekey — **当該テナントのみ**を現行 kid で再ラップする（admin）。
pub async fn rekey_secrets(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<impl IntoResponse, AppError> {
    require_admin_role(principal.role)?;
    let tenant = &principal.tenant_id;
    let keyring = state.secret_keyring();
    let active_kid = keyring.active_kid().to_string();

    // 対象の抽出は SECURITY DEFINER 関数（faas_app は FORCE RLS 下で巡回できない）。
    // **テナント引数を取る版**を使う（`_all` は背景ジョブ専用で HTTP からは呼ばない）。
    let targets = db::secrets_stale_kek(state.pool(), tenant, &active_kid).await?;

    let mut rewrapped = 0usize;
    for secret_id in targets {
        // 各 secret を独立した tx で処理する（1 件の失敗が他を巻き込まない）。
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, tenant).await?;

        let Some(meta) = db::find_secret_meta_by_id(&mut *tx, tenant, &secret_id).await? else {
            continue;
        };
        let Some(current) =
            db::find_secret_version(&mut *tx, tenant, &secret_id, meta.current_version).await?
        else {
            continue;
        };
        if current.kek_kid == active_kid {
            continue; // 既に現行 kid（並行実行での二重処理）。
        }

        let next_version = meta.current_version + 1;
        // **値の平文をメモリに載せない**: DEK を旧 KEK で unwrap → 新 KEK で wrap するだけで、
        // ciphertext / nonce / value_len はそのままコピーされる。
        let rewrapped_env = secrets::rewrap(
            keyring,
            &current,
            tenant,
            &secret_id,
            meta.current_version,
            next_version,
        )
        .map_err(map_secret_error)?;

        db::insert_secret_version(
            &mut *tx,
            tenant,
            &secret_id,
            next_version,
            &rewrapped_env,
            "rekey",
            principal.user_id.as_deref(),
        )
        .await?;
        db::bump_secret_current_version(&mut *tx, tenant, &secret_id, next_version).await?;

        db::insert_audit_log(
            &mut *tx,
            tenant,
            principal.user_id.as_deref(),
            "secret_rekeyed",
            Some(&secret_id),
            Some(&secrets::audit_detail(&meta.name, next_version, "rekey")),
        )
        .await?;

        tx.commit().await?;
        rewrapped += 1;
    }

    tracing::info!(%active_kid, rewrapped, "secrets re-wrapped with the active KEK");
    Ok(Json(RekeyResponse { rewrapped }))
}
