//! HTTP ハンドラ (M1/M2)。
//!
//! ルート:
//! - GET  /healthz                       … 認証不要・liveness
//! - POST /components                    … Component メタデータのみ作成 (M2: version は別途)
//! - POST /components/{id}/versions      … アップロード+検証+デプロイ (multipart, §6.2)
//! - POST /invoke                        … executions に pending INSERT → JobMessage publish → 202
//! - GET  /executions/{id}               … 実行状態の参照
//!
//! 認証は `auth::authenticate` 層が /healthz・/auth/login・POST /admin/tenants
//! 以外へ適用し、`Principal` を確立する。テナントは principal から解決する (§6.0)。

use axum::extract::{Multipart, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use faas_shared::{
    component_object_key, invoke_subject, new_component_id, new_execution_id, new_tenant_id,
    new_token_id, new_user_id, new_version_id, ExecutionStatus, FaasError, JobClaims, JobMessage,
    ResourceLimits, Role, Scope,
};
use sha2::{Digest, Sha256};
use sqlx::Acquire as _;

use crate::admission::{self, Decision};
use crate::auth::{hash_token, Principal};
use crate::authz::{require_admin_role, resolve_token_scopes};
use crate::crypto::{generate_secret, hash_password};
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// GET /healthz / /readyz / /metrics  (M4a, §3.8)
// ---------------------------------------------------------------------------

/// liveness。認証不要。プロセス生存のみを表現し、DB / NATS / Store に **触れない**（§3.8）。
///
/// kube-probe の liveness は「再起動を引き起こす」性質上、外部依存の一時障害でループ再起動
/// しないよう DB-free / NATS-free に保つ。readiness（依存疎通）は `/readyz` を使う。
pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// readiness（M4a, §3.8）。認証不要。DB / NATS / Store に軽量 ping して 200 or 503 を返す。
///
/// 仕様（fail-closed）:
/// - DB: `SELECT 1` を 500ms タイムアウトで一度だけ実行。
/// - NATS: クライアントの `connection_state()` を覗き Connected か確認。
/// - Store: `store.ping()`（Redis なら PING、DegradedStore は Ok）。
///
/// いずれか 1 つでも失敗・タイムアウトしたら 503 を返し、レスポンスボディに per-hop の状態
/// （"ok" / エラーメッセージ）を JSON で並べる。kubelet / LB が NotReady とみなして該当 Pod を
/// 退避できるようにする。NATS の購読遅延（subscriber lag）は readiness に巻き込まない
/// （NATS への接続性のみが /readyz の対象。subscriber lag は別メトリクスで観測する責務）。
pub async fn readyz(State(state): State<AppState>) -> Response {
    let db = check_db_ready(state.pool()).await;
    let nats = check_nats_ready(state.nats());
    let store = check_store_ready(state.store()).await;
    build_readyz_response(db, nats, store)
}

/// M4a (§3.8): DB の readiness ping（`SELECT 1` を 500ms タイムアウト付きで一度だけ）。
///
/// 別関数に切り出すのは、ユニットテストで「接続できない / 閉じたプール」に対して 503 系の
/// `Err` が返ることを直接検証するため。ハンドラ本体（[`readyz`]）は AppState を要求するため
/// テスト時のセットアップが重く、肝心の DB-down 経路を覆えなくなる。
async fn check_db_ready(pool: &sqlx::PgPool) -> Result<(), String> {
    use std::time::Duration;
    match tokio::time::timeout(
        Duration::from_millis(500),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("db: {e}")),
        Err(_) => Err("db: timeout".to_string()),
    }
}

/// NATS の readiness 判定（接続状態のみ）。Connected 以外は NotReady として `Err`。
fn check_nats_ready(client: &async_nats::Client) -> Result<(), String> {
    match client.connection_state() {
        async_nats::connection::State::Connected => Ok(()),
        other => Err(format!("nats: not connected ({other:?})")),
    }
}

/// Store の readiness 判定（`ping()` を直接呼ぶ）。
async fn check_store_ready(store: &dyn crate::store::Store) -> Result<(), String> {
    match store.ping().await {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("store: {e}")),
    }
}

/// per-hop の判定結果から 200 or 503 レスポンスを構築する（M4a, §3.8）。
///
/// 1 つでも失敗があれば 503。ボディは `{"db": "ok"|<err>, "nats": ..., "store": ...}`。
fn build_readyz_response(
    db: Result<(), String>,
    nats: Result<(), String>,
    store: Result<(), String>,
) -> Response {
    let body = json!({
        "db": match &db { Ok(()) => json!("ok"), Err(e) => json!(e) },
        "nats": match &nats { Ok(()) => json!("ok"), Err(e) => json!(e) },
        "store": match &store { Ok(()) => json!("ok"), Err(e) => json!(e) },
    });

    let all_ok = db.is_ok() && nats.is_ok() && store.is_ok();
    if all_ok {
        (StatusCode::OK, Json(body)).into_response()
    } else {
        // 失敗詳細はログにも残す（503 のたびに stderr に出して観測しやすくする）。
        tracing::warn!(?db, ?nats, ?store, "readyz: not ready");
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    }
}

/// Prometheus exposition（M4a, §3.8）。認証不要 / 認可不要（内部ネット越し前提）。
///
/// `Registry::gather()` を呼んで全 collector の現在値を集め、`TextEncoder` で text/plain 形式に
/// 書き出して返す。`/metrics` を外部公開する運用では、ルータ前段の Ingress / プロキシ層で
/// 認可をかけること（本スライスでは scaffolding を優先）。
pub async fn metrics(State(state): State<AppState>) -> Response {
    let (headers, body) = state.metrics().render();
    (StatusCode::OK, headers, body).into_response()
}

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
    let mut resource_limits: ResourceLimits = ResourceLimits::default();

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

    if let Some(caps) = &capabilities {
        // 宣言値は監査・観測用にログするのみ（保存・付与には使わない, §4.4）。
        tracing::debug!(declared = ?caps, imports = ?validated.imports, "client-declared capabilities (not trusted; persisting resolved approved set)");
    }
    // §4.4 MUST: クライアント宣言値ではなく、承認集合と照合して解決した import を保存する。
    let capabilities_json = json!(validated.approved_imports);

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

    db::set_active_version(&mut *tx, tenant, &component.id, &version_id).await?;

    tx.commit().await?;

    tracing::info!(
        component = %component.name,
        %version,
        sha256 = %validated.sha256,
        size_bytes = validated.size_bytes,
        "version uploaded and activated"
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
// POST /invoke
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct InvokeRequest {
    /// 起動する Component 名。
    pub component: String,
    /// インライン入力。worker は serde_json::to_vec(input) をハンドラへ渡す。
    /// `input_ref` と排他（§6.3）。
    #[serde(default)]
    pub input: Value,
    /// M3d (§3.4 / §5.2 / §6.3): 大入力の退避参照。`POST /uploads` で予約した
    /// `execution_id` から導出した `tenants/{caller_tenant}/io/{execution_id}/input` に
    /// **完全一致** しなければならない（prefix 一致不可・cross-exec/cross-tenant 不可）。
    /// `input` と排他。指定時は `execution_id` も必須。
    #[serde(default)]
    pub input_ref: Option<String>,
    /// M3d (§6.3): `POST /uploads` で予約済みの execution_id。`input_ref` 使用時は必須
    /// （本 API はこの id で executions 行を INSERT する。新規採番はしない）。未指定時は
    /// サーバが新規採番する（インライン invoke）。
    #[serde(default)]
    pub execution_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InvokeResponse {
    pub execution_id: String,
    pub status: ExecutionStatus,
}

/// M4a (§3.8): 全ログを execution_id / tenant_id で相関できるよう、ハンドラ全体を `invoke` span で
/// 包む。`execution_id` は本関数内で確定する（new_execution_id() または予約済み id）ため、
/// span 自体は最初は `Empty` で開き、確定後に `Span::current().record(...)` で詰める。
/// `#[tracing::instrument]` は引数フィールドをそのまま流すと巨大ボディが入るので、`skip_all`。
#[tracing::instrument(skip_all, fields(execution_id = tracing::field::Empty, tenant_id = %principal.tenant_id))]
pub async fn invoke(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    JsonBody(req): JsonBody<InvokeRequest>,
) -> Result<Response, AppError> {
    let tenant = &principal.tenant_id;

    // M4a (§3.8): per-tenant の invoke 受付数を観測する（admission の rate-limit より「呼ばれた回数」
    // を観測したいので、429 で弾く前に inc する）。429 で弾かれた件は admission_rejections_total が
    // 別に増えるため、(tenant_invoke_total - admission_rejections_total[kind=*]) で許可数を導出できる。
    state
        .metrics()
        .tenant_invoke_total
        .with_label_values(&[tenant.as_str()])
        .inc();

    // --- M4d (§8): per-tenant 上書きを反映した admission パラメータを解決する ---
    // tenants.quotas JSONB を 1 query で引き、グローバル既定（state.admission()）にマージする。
    // 上書きは「グローバル既定 → テナント上書き」優先順位（仕様 §8）でフィールドごとに適用。
    // PK lookup 1 本（sub-ms）なのでホットパスで毎回引いて短 TTL キャッシュは置かない（M4d）。
    // 注: 認証 middleware が tenants 行の存在は保証済み（不存在は 500 で短絡される）。
    let tenant_overrides = match db::load_tenant_status_and_quotas(state.pool(), tenant).await? {
        Some((_status, q)) => q,
        // principal あり + 行なし は middleware で 500 になるためここには来ない。
        // 来た場合は防御的にグローバル既定を使う（インシデント回避）。
        None => db::TenantQuotaOverrides::default(),
    };
    let resolved = state
        .admission()
        .resolve_for_tenant(tenant, &tenant_overrides);

    // --- admission ゲート 1: token-bucket レート制限 (§8) ---
    // 最も安い拒否（DB アクセス前）。fail-open: ストア到達不能なら admit + 縮退記録（§8）。
    // 超過は 429 + Retry-After。
    let now_ms = crate::store::now_unix_millis();
    if let Decision::Rejected(r) =
        admission::check_rate_limit(&state, tenant, &resolved, now_ms).await
    {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["rate_limited"])
            .inc();
        return Ok(r.into_response());
    }

    // --- 大入力 input_ref の検証 + execution_id の確定 (§3.4 / §5.2 / §6.3) ---
    // `input`（インライン）と `input_ref`（Storage 退避）は排他。`input_ref` 指定時は予約済み
    // `execution_id` も必須で、`input_ref` は `tenants/{caller_tenant}/io/{execution_id}/input` に
    // **完全一致** しなければならない（prefix/cross-exec/cross-tenant は 422）。完全一致を通った
    // キーだけを worker へ presign する（worker はそのキー以外を読まない, §3.4 MUST NOT）。
    let input_ref: Option<String> = match req.input_ref.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    // インライン input と input_ref は排他（§6.3）。input が非 null かつ input_ref も在れば 422。
    if input_ref.is_some() && !req.input.is_null() {
        return Err(FaasError::InvalidRequest(
            "`input` (inline) and `input_ref` (storage) are mutually exclusive".into(),
        )
        .into());
    }

    // execution_id を確定する: input_ref があれば予約済み id を使い完全一致検証、無ければ新規採番。
    let execution_id: String = if let Some(ref_key) = input_ref.as_deref() {
        let reserved = req
            .execution_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                FaasError::InvalidRequest(
                    "execution_id (reserved by POST /uploads) is required when input_ref is set"
                        .into(),
                )
            })?;
        validate_reserved_execution_id(reserved)?;
        // §3.4 MUST: input_ref を当該テナント・当該 execution の入力キーへ完全一致検証する。
        validate_input_ref(ref_key, tenant, reserved)?;
        reserved.to_string()
    } else {
        // input_ref が無いのに execution_id だけ指定された場合は無視せず明示拒否する
        // （予約済み id の使用は input_ref 経路に限る; 任意 id 指定で行を作らせない）。
        if req
            .execution_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
        {
            return Err(FaasError::InvalidRequest(
                "execution_id may only be supplied together with input_ref (a reserved upload)"
                    .into(),
            )
            .into());
        }
        new_execution_id()
    };

    // M4a: span に execution_id を記録する。ここから下のログは tracing の span 機構で自動的に
    // `execution_id=...` を相関フィールドとして保持する（JSON log では各イベントのトップレベル
    // フィールドになる; flatten_event + with_current_span 設定により）。
    tracing::Span::current().record("execution_id", execution_id.as_str());

    // --- 冪等性 layer 1: Idempotency-Key (§6.6) ---
    // 任意ヘッダ。在れば形式検証し、リクエスト body の正準 hash を計算しておく。
    let idem_key: Option<String> = match headers.get("Idempotency-Key").map(|v| v.to_str()) {
        Some(Ok(v)) => {
            validate_idempotency_key(v)?;
            Some(v.to_string())
        }
        Some(Err(_)) => {
            return Err(FaasError::InvalidRequest("Idempotency-Key must be ASCII".into()).into());
        }
        None => None,
    };
    // 冪等 body hash は (component, input, input_ref) を正準化する。input_ref を含めることで、
    // 同一キー + 異なる退避入力（別オブジェクト）を body 不一致（409）として正しく弁別する。
    let request_hash: Option<String> = idem_key
        .as_ref()
        .map(|_| invoke_request_hash(&req.component, &req.input, input_ref.as_deref()));

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;

    // 既存キーの fast path: 同一 body → 既存返却 / 異 body → 409（§6.6 decision #4）。
    // 冪等ヒットは **新規 in-flight を消費しない**（reserve はこの後）。
    if let (Some(key), Some(hash)) = (idem_key.as_deref(), request_hash.as_deref()) {
        if let Some((existing_id, existing_status, stored_hash)) =
            db::find_execution_by_idempotency_key(&mut *tx, tenant, key).await?
        {
            return finish_idempotent_hit(
                tx,
                tenant,
                existing_id,
                existing_status,
                stored_hash,
                hash,
            )
            .await
            .map(IntoResponse::into_response);
        }
    }

    // active component を解決し、active version の保存先・sha256 を引く (§6.3)。
    let component = db::find_component_by_name(&mut *tx, tenant, &req.component)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("component '{}'", req.component)))?;

    let active = db::active_version_storage(&mut *tx, tenant, &req.component)
        .await?
        .ok_or_else(|| {
            FaasError::InvalidRequest(format!(
                "component '{}' has no active version",
                req.component
            ))
        })?;

    // --- admission ゲート 2: in-flight 同時実行 reserve (§8) ---
    // 冪等 fast-path miss を確認し、本当に新規 enqueue する確度が高まった所で原子予約する
    // （INCR-cmp-condDECR を 1 往復, TOCTOU 無し）。上限超過は 429。fail-open: ストア到達
    // 不能なら admit + 縮退記録（その場合 reserved=false で、終端 DECR の対象にならない）。
    //
    // `reserved` が true のときは +1 済みであり、この invoke が **新規 pending を確定して
    // publish するまでに失敗したら必ず release（-1）** しなければならない（leak 防止）。
    // 正常系の DECR は subscriber の verified finalize（唯一の終端 writer）が行う。reaper が
    // 取りこぼしを DB COUNT で再同期する安全網。
    let (decision, reserved) = admission::reserve_inflight(&state, tenant, &resolved).await;
    if let Decision::Rejected(r) = decision {
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["concurrency_limit"])
            .inc();
        return Ok(r.into_response());
    }
    // ここから先で early-return する経路は、reserved のとき release してから返す。
    macro_rules! release_if_reserved {
        () => {
            if reserved {
                if let Err(e) = state.store().release_inflight(tenant).await {
                    tracing::warn!(tenant = %tenant, error = %e, "failed to release in-flight slot on abort");
                }
            }
        };
    }

    // active version の本体へ短命 presigned GET URL を発行する (§3.4)。
    // storage_uri は Object Storage 上のオブジェクトキー。
    let wasm_url = match state
        .storage()
        .presign_get(&active.storage_uri, state.presign_ttl())
        .await
    {
        Ok(u) => u,
        Err(e) => {
            release_if_reserved!();
            return Err(e.into());
        }
    };

    // 大入力がある場合、退避済み入力オブジェクト（完全一致検証済みキー）への **そのキー限定**
    // の短命 presigned GET URL を発行する (§3.4)。worker はこの URL からのみ入力を読み、他キーは
    // 読まない。インライン invoke では None（worker は JobMessage.input を使う）。
    let input_url: Option<String> = match input_ref.as_deref() {
        Some(key) => match state.storage().presign_get(key, state.presign_ttl()).await {
            Ok(u) => Some(u),
            Err(e) => {
                release_if_reserved!();
                return Err(e.into());
            }
        },
        None => None,
    };

    // --- 署名トークンを mint (M3c, §3.3) ---
    // exp は壁時計上限 + TTL 定数から計算する（worker の consumer ack_wait/max_deliver と同一定数）。
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + state.token_exp_offset_secs(active.max_wall_time_ms);
    let kid = state.signer().kid().to_string();
    let claims = JobClaims {
        execution_id: execution_id.clone(),
        tenant_id: tenant.to_string(),
        version_id: active.version_id.clone(),
        kid: kid.clone(),
        iat,
        exp,
    };
    let job_token = state.signer().sign(&claims);

    // 1) executions に pending を記録 (worker/result が後で遷移させる)。冪等列も併せて保存する。
    //
    // 並行同一 key 競合の backstop (§6.6 #5): 部分 UNIQUE 違反(23505) を捕捉し、再 SELECT して
    // 同一 body→既存返却 / 異 body→409 を判定する（index が権威; SELECT は fast path）。
    //
    // INSERT は **savepoint**（sqlx の nested begin = SAVEPOINT）で包む。Postgres では
    // 失敗ステートメントが tx 全体を abort し、以後のコマンドは 25P02 で弾かれる。savepoint
    // 内で INSERT し、23505 なら savepoint を rollback して abort 部分状態を解消してから、
    // 外側 tx で再 SELECT する（こうしないと再 SELECT が aborted-tx エラーになり 500 になる）。
    let insert_result = {
        let mut sp = match tx.begin().await {
            Ok(sp) => sp,
            Err(e) => {
                release_if_reserved!();
                return Err(e.into());
            }
        };
        let r = db::insert_pending_execution_with_provenance(
            &mut *sp,
            &execution_id,
            tenant,
            &component.id,
            &active.version_id,
            &req.input,
            idem_key.as_deref(),
            request_hash.as_deref(),
            Some(&kid),
            input_ref.as_deref(),
        )
        .await;
        match r {
            Ok(()) => {
                // savepoint を release（外側 tx に取り込む）。
                if let Err(e) = sp.commit().await {
                    release_if_reserved!();
                    return Err(e.into());
                }
                Ok(())
            }
            Err(e) => {
                // savepoint を rollback して abort 部分状態を解消する（外側 tx は生き続ける）。
                if let Err(re) = sp.rollback().await {
                    release_if_reserved!();
                    return Err(re.into());
                }
                Err(e)
            }
        }
    };

    if let Err(e) = insert_result {
        // INSERT が失敗した = この invoke は **新規 pending 行を作っていない**。予約した
        // in-flight スロットは終端 DECR の対象にならない（pending 行が無いため）ので、
        // どの分岐で抜けるにせよ必ず release する（leak 防止）。冪等ヒット側のスロットは
        // 既存 pending 行に紐づく別予約であり、これとは独立。
        if is_unique_violation(&e) {
            if let (Some(key), Some(hash)) = (idem_key.as_deref(), request_hash.as_deref()) {
                if let Some((existing_id, existing_status, stored_hash)) =
                    db::find_execution_by_idempotency_key(&mut *tx, tenant, key).await?
                {
                    release_if_reserved!();
                    return finish_idempotent_hit(
                        tx,
                        tenant,
                        existing_id,
                        existing_status,
                        stored_hash,
                        hash,
                    )
                    .await
                    .map(IntoResponse::into_response);
                }
            }
        }
        release_if_reserved!();
        return Err(e.into());
    }

    // tx を確定してから publish する（NATS publish をトランザクション境界の外に出す:
    // ネットワーク publish 中に tx/接続を保持しない）。
    // commit 後は pending 行が確定し、in-flight スロットは「その pending 行が保持する」状態に
    // 移る（DB COUNT がそれを真実として数える）。よって以後の失敗（publish 失敗等）では
    // **release しない** ——スロットは pending 行に紐づき、subscriber の finalize で DECR され、
    // 取りこぼしは reaper の DB COUNT 再同期が補正する。
    if let Err(e) = tx.commit().await {
        release_if_reserved!();
        return Err(e.into());
    }

    // 2) JobMessage を invoke_subject へ publish。worker は wasm_url から本体を取得し、
    //    wasm_sha256 をキャッシュ/事前コンパイルのキーにする (§3.6)。
    let job = JobMessage {
        execution_id: execution_id.clone(),
        tenant_id: tenant.to_string(),
        component: req.component,
        version: active.version,
        wasm_sha256: active.wasm_sha256,
        wasm_url,
        // インライン入力。大入力時（input_url が在るとき）は worker が input_url を優先する。
        input: req.input,
        // M3d (§3.4): 大入力の退避オブジェクトへの、そのキー限定の短命 presigned GET URL。
        input_url,
        // M3c: control-plane が署名した不透明トークン。worker は verbatim に echo する。
        job_token,
    };
    let payload = serde_json::to_vec(&job)?;

    // --- 冪等性 layer 3: Nats-Msg-Id = execution_id (§6.6) ---
    // JetStream publish で per-stream の重複排除ウィンドウに execution_id を渡す。
    // CP のリトライで同一 execution_id を再 publish しても二重 enqueue されない。
    //
    // M4d (§8): publish 自体は MaxAckPending 制限を直接 ack エラーとして返さない（JetStream の
    // MaxAckPending は consumer 側の「配送済み未 ack」の頭打ち）。が、ストリーム書き込みの
    // TimedOut / BrokenPipe / Other は **下流が詰まっている**シグナルなので、500（リトライ抑制）
    // ではなく 429（バックオフ後リトライ）で返す方がクライアント挙動として正しい
    // （§8 line 749「キュー滞留時もバックプレッシャとして 429」MUST）。pending 行は既に commit
    // 済み（孤児だが reaper の stuck-execution sweeper が deadline で failed に倒す = 二段救済）。
    let mut nats_headers = async_nats::HeaderMap::new();
    nats_headers.insert("Nats-Msg-Id", execution_id.as_str());
    let ack = match state
        .jetstream()
        .publish_with_headers(invoke_subject(tenant), nats_headers, payload.into())
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant,
                error = %e,
                kind = ?e.kind(),
                "jetstream publish failed; returning 429 backpressure"
            );
            state
                .metrics()
                .admission_rejections_total
                .with_label_values(&["publish_backpressure"])
                .inc();
            return Ok(admission::RateLimited::publish_backpressure().into_response());
        }
    };
    // PublishAck を待ち、stream が受理したことを確認する（落ちてもジョブは DB に記録済み）。
    if let Err(e) = ack.await {
        tracing::warn!(
            tenant = %tenant,
            error = %e,
            kind = ?e.kind(),
            "jetstream publish ack failed; returning 429 backpressure"
        );
        state
            .metrics()
            .admission_rejections_total
            .with_label_values(&["publish_backpressure"])
            .inc();
        return Ok(admission::RateLimited::publish_backpressure().into_response());
    }

    // 3) 202 Accepted。
    Ok((
        StatusCode::ACCEPTED,
        Json(InvokeResponse {
            execution_id,
            status: ExecutionStatus::Pending,
        }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// POST /uploads — 大入力アップロード用の署名付き PUT URL 発行 (§3.4 / §5.2 / §6.4)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateUploadResponse {
    /// 予約した execution_id（採番のみ。executions 行は INSERT しない）。
    pub execution_id: String,
    /// 後続 /invoke で指定する input_ref（= 退避先オブジェクトキー）。
    pub input_ref: String,
    /// 単一キー限定・短 TTL の presigned PUT URL。クライアントは本体をここへ PUT する。
    pub upload_url: String,
    /// presign の失効時刻（RFC3339）。
    pub expires_at: String,
}

/// POST /uploads — 大入力を invoke 前に退避するための署名付き PUT URL を発行する (§5.2 / §6.4)。
///
/// 手順 (§6.0):
/// 1. `execution_id` を **予約**（採番）する。オブジェクトキー
///    `tenants/{caller_tenant}/io/{execution_id}/input` を確定する。
/// 2. そのキー限定・短 TTL の presigned PUT URL を返す（§3.4 の write 資格と同条件）。
///
/// 重要な不変条件（§6.0 MUST）:
/// - **executions 行を INSERT しない**（行は component/version が確定する /invoke 時点で INSERT する）。
/// - **in-flight INCR をしない**（uploads は同時実行を消費しない。INCR は /invoke 時点）。
///
/// 予約のみで未 INSERT の execution は executions 行を持たないため、`max_concurrent_executions`
/// （対象は pending+running）には計上されない。孤児（PUT 済み未使用）は presign TTL + GC で回収する
/// （§6.0; バケットライフサイクル / reaper。本スライスでは TTL のみで物理 GC は運用側 TODO）。
///
/// 認可: invoke スコープ（ルータで route_layer 済み）。テナントは principal から解決する（§6.0）。
pub async fn create_upload(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    // 1) execution_id を予約（採番のみ。INSERT しない）。
    let execution_id = new_execution_id();
    let input_ref = faas_shared::io_input_key(tenant, &execution_id);

    // 2) 当該キー限定・短 TTL の presigned PUT URL を発行する（§3.4 write 資格）。
    let ttl = state.upload_presign_ttl();
    let upload_url = state.storage().presign_put(&input_ref, ttl).await?;
    let expires_at = chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();

    tracing::info!(%execution_id, tenant = %tenant, "reserved upload (presigned PUT for input)");

    Ok((
        StatusCode::CREATED,
        Json(CreateUploadResponse {
            execution_id,
            input_ref,
            upload_url,
            expires_at: expires_at.to_rfc3339(),
        }),
    ))
}

/// 既存の冪等ヒットに対する応答を確定する（同一 body→既存返却 / 異 body→409）。
///
/// `tx` は principal のテナントで GUC が設定済み（=権威テナント）。新規 INSERT も publish も
/// しないが、409（key 再利用 + body 不一致）のときだけ同一 tx で `idempotency_conflict` を
/// audit_logs に追記してから commit する（§3.7。tenant_id は GUC と一致し WITH CHECK を通す）。
async fn finish_idempotent_hit(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    existing_id: String,
    existing_status: String,
    stored_hash: Option<String>,
    current_hash: &str,
) -> Result<(StatusCode, Json<InvokeResponse>), AppError> {
    // body hash が一致しなければ 409（同一キーで異なるリクエスト body）。
    if stored_hash.as_deref() != Some(current_hash) {
        // 監査追記（best-effort: 失敗しても 409 応答は返す）。生 body/hash は載せない。
        let detail = json!({ "reason": "request_body_hash_mismatch" });
        let _ = db::insert_audit_log(
            &mut *tx,
            tenant,
            None,
            "idempotency_conflict",
            Some(&existing_id),
            Some(&detail),
        )
        .await;
        let _ = tx.commit().await;
        return Err(FaasError::Conflict(
            "idempotency key reused with a different request body".into(),
        )
        .into());
    }
    let _ = tx.commit().await;
    // 既存 execution の現在状態をそのまま返す（未知文字列は Pending 扱い）。
    let status = parse_execution_status(&existing_status).unwrap_or(ExecutionStatus::Pending);
    Ok((
        StatusCode::ACCEPTED,
        Json(InvokeResponse {
            execution_id: existing_id,
            status,
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /executions/{id}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ExecutionResponse {
    pub execution_id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<Value>,
    /// M3d (§6.4): 大入力の退避参照（インラインなら null）。
    pub input_ref: Option<String>,
    /// M3d (§6.4): 大出力の退避参照（インラインなら null）。
    pub output_ref: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

pub async fn get_execution(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    // GET も RLS 下では app.tenant_id を SELECT 実行接続にセットする必要がある
    // （GUC 未設定の bare 接続は fail-closed で ERROR）。
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let row = db::get_execution(&mut *tx, tenant, &id)
        .await?
        .ok_or_else(|| FaasError::NotFound(format!("execution '{id}'")))?;
    tx.commit().await?;

    Ok(Json(ExecutionResponse {
        execution_id: row.id,
        tenant_id: row.tenant_id,
        component_id: row.component_id,
        version_id: row.version_id,
        status: row.status,
        input: row.input,
        output: row.output,
        error: row.error,
        input_ref: row.input_ref,
        output_ref: row.output_ref,
        created_at: row.created_at.to_rfc3339(),
        started_at: row.started_at.map(|t| t.to_rfc3339()),
        finished_at: row.finished_at.map(|t| t.to_rfc3339()),
    }))
}

// ---------------------------------------------------------------------------
// GET /usage — テナント利用量参照 (M5, §15 / §6.0, read スコープ)
// ---------------------------------------------------------------------------

/// `GET /usage` のクエリパラメータ。
///
/// `from`/`to` は `YYYY-MM-DD`（UTC 日境界）。`usage_rollups` は UTC 日次粒度で集計されるため
/// レスポンスも UTC 日次（period の両端含む）になる。既定は `to`=今日(UTC) / `from`=`to`-30 日。
/// `component_id` 指定時はその component のみに絞り込む（未指定なら全 component）。
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub component_id: Option<String>,
}

/// `GET /usage` 応答。`totals` は `by_component` を畳んだ全体集計（SUM 列は和、peak は max）。
#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub tenant_id: String,
    /// 集計対象期間の開始（UTC 日, 含む）。`YYYY-MM-DD`。
    pub from: String,
    /// 集計対象期間の終了（UTC 日, 含む）。`YYYY-MM-DD`。
    pub to: String,
    pub totals: UsageTotals,
    pub by_component: Vec<UsageByComponent>,
}

/// 期間全体の集計値。
///
/// 解釈（§15 設計）: `invocation_count` は全終端（succeeded/failed/timeout）で +1 される。
/// リソース指標（`cpu_fuel_used`/`wall_time_ms`/`output_bytes` は SUM, `peak_memory_bytes_max`
/// は MAX）は計測済み result のみ加算される（DLQ/timeout は count を立てつつリソースは 0 加算）。
/// 0007 migration 適用前の execution は計量を持たず rollup に現れない（additive・backfill 無し）。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UsageTotals {
    pub invocation_count: i64,
    pub cpu_fuel_used: i64,
    pub wall_time_ms: i64,
    pub peak_memory_bytes_max: i64,
    pub output_bytes: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timeout_count: i64,
}

/// component 別の集計 1 件。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UsageByComponent {
    pub component_id: String,
    pub invocation_count: i64,
    pub cpu_fuel_used: i64,
    pub wall_time_ms: i64,
    pub peak_memory_bytes_max: i64,
    pub output_bytes: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timeout_count: i64,
}

/// `from`/`to` クエリ文字列を `NaiveDate` に解決する（純関数・時計を引数化してテスト可能）。
///
/// 既定: `to`=`today` / `from`=`to`-30 日。指定時は `YYYY-MM-DD` をパースし、不正は
/// [`FaasError::InvalidRequest`]（→ 400）。`from > to` も 400 で弾く（空でない範囲を保証）。
fn resolve_usage_range(
    from: Option<&str>,
    to: Option<&str>,
    today: chrono::NaiveDate,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate), FaasError> {
    let parse = |label: &str, s: &str| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| FaasError::InvalidRequest(format!("{label} must be YYYY-MM-DD")))
    };

    let to = match to {
        Some(s) => parse("to", s)?,
        None => today,
    };
    let from = match from {
        Some(s) => parse("from", s)?,
        None => to - chrono::Duration::days(30),
    };

    if from > to {
        return Err(FaasError::InvalidRequest("from must not be after to".into()));
    }
    Ok((from, to))
}

/// `by_component` を期間全体の `totals` へ畳む（純関数）。SUM 列は和、peak は max を取る。
fn fold_usage_totals(by_component: &[UsageByComponent]) -> UsageTotals {
    by_component.iter().fold(
        UsageTotals {
            invocation_count: 0,
            cpu_fuel_used: 0,
            wall_time_ms: 0,
            peak_memory_bytes_max: 0,
            output_bytes: 0,
            succeeded_count: 0,
            failed_count: 0,
            timeout_count: 0,
        },
        |mut acc, c| {
            acc.invocation_count += c.invocation_count;
            acc.cpu_fuel_used += c.cpu_fuel_used;
            acc.wall_time_ms += c.wall_time_ms;
            acc.peak_memory_bytes_max = acc.peak_memory_bytes_max.max(c.peak_memory_bytes_max);
            acc.output_bytes += c.output_bytes;
            acc.succeeded_count += c.succeeded_count;
            acc.failed_count += c.failed_count;
            acc.timeout_count += c.timeout_count;
            acc
        },
    )
}

/// GET /usage — テナント利用量参照 (M5, §15 / §6.0, read スコープ)。
///
/// `principal.tenant_id` を唯一の権威値として使う（cross-tenant path を持たず IDOR 面を増やさない）。
/// RLS 下では SELECT 実行接続に `app.tenant_id` をセットする必要があるため tx を張って GUC を設定する
/// （GUC 未設定の bare 接続は fail-closed で ERROR）。WHERE にも tenant_id をバインドして二重防御する。
pub async fn get_usage(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<UsageQuery>,
) -> Result<impl IntoResponse, AppError> {
    let tenant = &principal.tenant_id;

    let today = chrono::Utc::now().date_naive();
    let (from, to) = resolve_usage_range(q.from.as_deref(), q.to.as_deref(), today)?;

    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, tenant).await?;
    let rows = db::get_usage_rollups(&mut *tx, tenant, from, to, q.component_id.as_deref()).await?;
    tx.commit().await?;

    let by_component: Vec<UsageByComponent> = rows
        .into_iter()
        .map(|r| UsageByComponent {
            component_id: r.component_id,
            invocation_count: r.invocation_count,
            cpu_fuel_used: r.cpu_fuel_used,
            wall_time_ms: r.wall_time_ms,
            peak_memory_bytes_max: r.peak_memory_bytes_max,
            output_bytes: r.output_bytes,
            succeeded_count: r.succeeded_count,
            failed_count: r.failed_count,
            timeout_count: r.timeout_count,
        })
        .collect();

    let totals = fold_usage_totals(&by_component);

    Ok(Json(UsageResponse {
        tenant_id: tenant.clone(),
        from: from.format("%Y-%m-%d").to_string(),
        to: to.format("%Y-%m-%d").to_string(),
        totals,
        by_component,
    }))
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

    db::set_active_version(&mut *tx, tenant, &component_id, &target_version_id).await?;

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

// ---------------------------------------------------------------------------
// POST /admin/tenants — テナント作成 (bootstrap system-admin, §3.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    /// グローバル一意な人間可読 slug（subject-safe 化はしないが UNIQUE）。
    pub slug: String,
    pub name: String,
    /// 最初の admin ユーザの email（このテナント内）。
    pub admin_email: String,
    /// 最初の admin ユーザのパスワード（argon2id でハッシュして保存）。
    pub admin_password: String,
}

#[derive(Debug, Serialize)]
pub struct CreateTenantResponse {
    pub tenant_id: String,
    pub slug: String,
    pub name: String,
    /// 作成された最初の admin ユーザ。以後は `/auth/login` でトークンを取得する。
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
    if req.admin_email.trim().is_empty() {
        return Err(FaasError::InvalidRequest("admin_email must not be empty".into()).into());
    }
    if req.admin_password.is_empty() {
        return Err(FaasError::InvalidRequest("admin_password must not be empty".into()).into());
    }

    // テナント + 最初の admin ユーザを 1 トランザクションで作成（§9 bootstrap）。
    // M3b: bootstrap は Principal を持たない唯一の書き込み経路。users は FORCE RLS 下に
    // あるため、生成直後の tenant_id で同一 tx に GUC を設定してから bootstrap_tenant を
    // 呼ぶ（users INSERT の WITH CHECK を通すため。tenants には RLS は無い）。
    let tenant_id = new_tenant_id();
    let admin_user_id = new_user_id();
    let admin_password_hash = hash_password(&req.admin_password)?;
    let mut tx = state.pool().begin().await.map_err(AppError::from)?;
    db::set_tenant_guc(&mut tx, &tenant_id)
        .await
        .map_err(AppError::from)?;
    db::bootstrap_tenant(
        &mut tx,
        &tenant_id,
        req.slug.trim(),
        req.name.trim(),
        &admin_user_id,
        req.admin_email.trim(),
        &admin_password_hash,
    )
    .await
    .map_err(|e| map_unique_violation(e, "tenant slug already exists"))?;
    // §3.7: 最高権限の identity プロビジョニング。GUC は新 tenant_id に設定済みなので
    // WITH CHECK を通る。bootstrap 主体由来であることを示し、target は新 tenant_id。
    // admin_password は決して載せない。
    db::insert_audit_log(
        &mut *tx,
        &tenant_id,
        Some("bootstrap"),
        "tenant_created",
        Some(&tenant_id),
        Some(&json!({ "slug": req.slug.trim(), "admin_user_id": admin_user_id })),
    )
    .await
    .map_err(AppError::from)?;
    tx.commit().await.map_err(AppError::from)?;

    tracing::info!(tenant_id = %tenant_id, slug = %req.slug, admin_user_id = %admin_user_id, "tenant bootstrapped");
    Ok((
        StatusCode::CREATED,
        Json(CreateTenantResponse {
            tenant_id,
            slug: req.slug,
            name: req.name,
            admin_user_id,
            admin_email: req.admin_email,
        }),
    ))
}

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
    pub token: String,
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
            token: secret,
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

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// bootstrap トークンを定数時間で照合する（長さ込み）。
///
/// 空の期待値（未設定）は常に false（誤って全許可しない）。
fn bootstrap_token_matches(provided: &str, expected: &str) -> bool {
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

/// UNIQUE 制約違反 (重複 name / version 等) を 400 にマップする。それ以外は内部エラー。
fn map_unique_violation(e: sqlx::Error, msg: &str) -> AppError {
    if let sqlx::Error::Database(db_err) = &e {
        // Postgres unique_violation = 23505
        if db_err.code().as_deref() == Some("23505") {
            return FaasError::InvalidRequest(msg.into()).into();
        }
    }
    e.into()
}

/// Postgres の unique_violation (23505) かどうか（冪等 race の backstop 判定）。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505"))
}

/// 実行状態文字列を `ExecutionStatus` へパースする（冪等ヒット応答用）。未知は `None`。
fn parse_execution_status(s: &str) -> Option<ExecutionStatus> {
    match s {
        "pending" => Some(ExecutionStatus::Pending),
        "running" => Some(ExecutionStatus::Running),
        "succeeded" => Some(ExecutionStatus::Succeeded),
        "failed" => Some(ExecutionStatus::Failed),
        "timeout" => Some(ExecutionStatus::Timeout),
        _ => None,
    }
}

/// 大入力 `input_ref` を当該 invoke の execution に対して **完全一致** で検証する (§3.4 MUST)。
///
/// 受理条件は `input_ref == tenants/{caller_tenant}/io/{execution_id}/input` の **完全一致** のみ。
/// prefix 一致では不十分であり、同一テナント内の別 execution / 別ユーザの入力を指す `input_ref`
/// （cross-exec）も、別テナントの入力を指す `input_ref`（cross-tenant）も拒否する。worker へ同梱
/// する read 資格は invoke の execution_id から導出したキーに固定するため、ここを通った値だけが
/// presign される。純関数（DB/ストア非依存）でユニットテスト可能。
///
/// 違反は `InvalidRequest`（→ 422 相当）。`tenant` は権威テナント（principal 由来）、
/// `execution_id` は予約済み（`POST /uploads`）の id。
fn validate_input_ref(
    input_ref: &str,
    tenant: &str,
    execution_id: &str,
) -> faas_shared::Result<()> {
    let expected = faas_shared::io_input_key(tenant, execution_id);
    if input_ref == expected {
        Ok(())
    } else {
        Err(FaasError::InvalidRequest(format!(
            "input_ref must exactly match '{expected}' for this execution; \
             prefix/cross-execution/cross-tenant references are rejected"
        )))
    }
}

/// 予約済み execution_id の形式検証（`POST /uploads` が採番する `exec_*`）。
///
/// `input_ref` 経路では client が提示する値であり、キー空間（オブジェクトキー）に
/// 埋め込まれるため subject/key-safe（`/` や空白・パス traversal 文字を含まない）かつ
/// `exec_` プレフィックスを要求する。違反は `InvalidRequest`（→ 422 相当）。純関数。
fn validate_reserved_execution_id(id: &str) -> faas_shared::Result<()> {
    let ok = id.starts_with("exec_")
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(FaasError::InvalidRequest(
            "execution_id must be a server-reserved id of the form exec_[A-Za-z0-9_-]".into(),
        ))
    }
}

/// Idempotency-Key の形式検証 (§6.6 decision #4)。
///
/// 非空・最大 255 文字・charset `[A-Za-z0-9._-]` のみ許容する。違反は 400。
/// 純関数（DB 非依存）でユニットテスト可能。
fn validate_idempotency_key(key: &str) -> faas_shared::Result<()> {
    if key.is_empty() {
        return Err(FaasError::InvalidRequest(
            "Idempotency-Key must not be empty".into(),
        ));
    }
    if key.len() > 255 {
        return Err(FaasError::InvalidRequest(
            "Idempotency-Key must be at most 255 characters".into(),
        ));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(FaasError::InvalidRequest(
            "Idempotency-Key may only contain [A-Za-z0-9._-]".into(),
        ));
    }
    Ok(())
}

/// invoke リクエストの正準 hash（§6.6 layer 1 の body 一致判定）。
///
/// 意味的同一性は (component, input)。論理的に同じ body はキー順・空白に関わらず
/// 同一 hash になるよう、`{"component","input"}` を **キーをソートして** 正準化した
/// バイト列の sha256 hex を返す。serde_json::to_vec はキーをソートしないため、
/// `canonical_json_bytes` で明示的にソートして直列化する。
fn invoke_request_hash(component: &str, input: &Value, input_ref: Option<&str>) -> String {
    let v = json!({ "component": component, "input": input, "input_ref": input_ref });
    let bytes = canonical_json_bytes(&v);
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `serde_json::Value` を **オブジェクトキーを再帰的にソート** して正準バイト列へ直列化する。
///
/// serde_json の `to_vec` はオブジェクトキーをソートしないため（`preserve_order` 無効時は
/// 挿入順 or 任意順）、ここで BTreeMap を使って決定的に並べる。配列は順序保持、スカラは
/// serde_json の数値/文字列/真偽/null 表現をそのまま使う。これにより
/// `{"a":1,"b":2}` と `{"b":2,"a":1}` が同一バイト列になる。
fn canonical_json_bytes(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Object(map) => {
            out.push(b'{');
            // キーをソートして決定的に並べる。
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                // キーは JSON 文字列としてエスケープして出す（serde_json に委譲）。
                let key_bytes = serde_json::to_vec(*k).expect("string serialize never fails");
                out.extend_from_slice(&key_bytes);
                out.push(b':');
                write_canonical(&map[*k], out);
            }
            out.push(b'}');
        }
        Value::Array(arr) => {
            out.push(b'[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        // スカラ（数値・文字列・真偽・null）は serde_json の表現をそのまま使う。
        other => {
            let bytes = serde_json::to_vec(other).expect("scalar serialize never fails");
            out.extend_from_slice(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- GET /usage 純関数 ---

    /// from/to 未指定の既定: to=today / from=today-30d。
    #[test]
    fn usage_range_defaults_to_last_30_days() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        let (from, to) = resolve_usage_range(None, None, today).unwrap();
        assert_eq!(to, today);
        assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2026, 5, 24).unwrap());
    }

    /// from/to 明示指定はパースされ既定を上書きする。
    #[test]
    fn usage_range_parses_explicit_bounds() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        let (from, to) =
            resolve_usage_range(Some("2026-01-01"), Some("2026-01-31"), today).unwrap();
        assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        assert_eq!(to, chrono::NaiveDate::from_ymd_opt(2026, 1, 31).unwrap());
    }

    /// 不正な日付フォーマットは 400（InvalidRequest）。
    #[test]
    fn usage_range_rejects_malformed_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        assert!(matches!(
            resolve_usage_range(Some("2026/01/01"), None, today),
            Err(FaasError::InvalidRequest(_))
        ));
        assert!(matches!(
            resolve_usage_range(None, Some("not-a-date"), today),
            Err(FaasError::InvalidRequest(_))
        ));
    }

    /// from > to は空でない範囲を保証するため 400。
    #[test]
    fn usage_range_rejects_inverted_bounds() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        assert!(matches!(
            resolve_usage_range(Some("2026-02-01"), Some("2026-01-01"), today),
            Err(FaasError::InvalidRequest(_))
        ));
    }

    fn by_component(component_id: &str, inv: i64, cpu: i64, peak: i64) -> UsageByComponent {
        UsageByComponent {
            component_id: component_id.to_string(),
            invocation_count: inv,
            cpu_fuel_used: cpu,
            wall_time_ms: 0,
            peak_memory_bytes_max: peak,
            output_bytes: 0,
            succeeded_count: inv,
            failed_count: 0,
            timeout_count: 0,
        }
    }

    /// 畳み込み: SUM 列は和、peak は最大。
    #[test]
    fn fold_totals_sums_and_takes_max_peak() {
        let rows = vec![
            by_component("c1", 2, 100, 4096),
            by_component("c2", 3, 50, 8192),
        ];
        let totals = fold_usage_totals(&rows);
        assert_eq!(totals.invocation_count, 5);
        assert_eq!(totals.cpu_fuel_used, 150);
        assert_eq!(totals.peak_memory_bytes_max, 8192);
        assert_eq!(totals.succeeded_count, 5);
    }

    /// 空入力は全 0 の totals。
    #[test]
    fn fold_totals_empty_is_zero() {
        let totals = fold_usage_totals(&[]);
        assert_eq!(
            totals,
            UsageTotals {
                invocation_count: 0,
                cpu_fuel_used: 0,
                wall_time_ms: 0,
                peak_memory_bytes_max: 0,
                output_bytes: 0,
                succeeded_count: 0,
                failed_count: 0,
                timeout_count: 0,
            }
        );
    }

    #[test]
    fn bootstrap_token_matches_only_on_exact_value() {
        assert!(bootstrap_token_matches("s3cret", "s3cret"));
        assert!(!bootstrap_token_matches("s3cret", "s3cre"));
        assert!(!bootstrap_token_matches("s3cre", "s3cret"));
        assert!(!bootstrap_token_matches("wrong", "s3cret"));
    }

    #[test]
    fn bootstrap_empty_expected_never_matches() {
        // 未設定（空）の期待値は誤って全許可しない。
        assert!(!bootstrap_token_matches("", ""));
        assert!(!bootstrap_token_matches("anything", ""));
    }

    /// create_token のスコープ ceiling: caller=member相当が admin 要求すると 403。
    #[test]
    fn create_token_scope_ceiling_rejects_escalation() {
        let caller = vec![Scope::Read, Scope::Invoke, Scope::Deploy];
        let err = resolve_token_scopes(&[Scope::Admin], &caller, Role::Admin).unwrap_err();
        assert!(matches!(err, FaasError::Forbidden));
    }

    /// 対象ユーザが member なら admin スコープは付与不能（caller が admin でも）。
    #[test]
    fn create_token_target_role_ceiling_enforced() {
        let caller = vec![Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin];
        let err = resolve_token_scopes(&[Scope::Admin], &caller, Role::Member).unwrap_err();
        assert!(matches!(err, FaasError::Forbidden));
    }

    // ---- M3c 冪等性: Idempotency-Key 形式検証 (§6.6 decision #4) -----------------

    #[test]
    fn idempotency_key_accepts_valid_charset() {
        assert!(validate_idempotency_key("abcXYZ_0-9.key").is_ok());
        assert!(validate_idempotency_key("A").is_ok());
        // 255 文字ちょうどは許容。
        assert!(validate_idempotency_key(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn idempotency_key_rejects_invalid() {
        // 空。
        assert!(matches!(
            validate_idempotency_key(""),
            Err(FaasError::InvalidRequest(_))
        ));
        // 256 文字（上限超過）。
        assert!(matches!(
            validate_idempotency_key(&"a".repeat(256)),
            Err(FaasError::InvalidRequest(_))
        ));
        // 許可外文字（空白・スラッシュ・コロン・非 ASCII）。
        for bad in ["has space", "has/slash", "a:b", "café", "semi;colon"] {
            assert!(
                matches!(
                    validate_idempotency_key(bad),
                    Err(FaasError::InvalidRequest(_))
                ),
                "should reject {bad:?}"
            );
        }
    }

    // ---- M3c 冪等性: 正準 body hash (§6.6 layer 1) ------------------------------

    /// オブジェクトキー順が違っても同一 hash（論理的同一 body は同一 hash）。
    #[test]
    fn request_hash_is_key_order_independent() {
        let a = json!({"b": 2, "a": 1, "nested": {"y": 1, "x": 2}});
        let b = json!({"a": 1, "nested": {"x": 2, "y": 1}, "b": 2});
        assert_eq!(
            invoke_request_hash("echo", &a, None),
            invoke_request_hash("echo", &b, None)
        );
    }

    /// component / input が違えば hash も違う。
    #[test]
    fn request_hash_distinguishes_component_and_input() {
        let input = json!({"k": "v"});
        assert_ne!(
            invoke_request_hash("echo", &input, None),
            invoke_request_hash("other", &input, None)
        );
        assert_ne!(
            invoke_request_hash("echo", &json!({"k": "v1"}), None),
            invoke_request_hash("echo", &json!({"k": "v2"}), None)
        );
    }

    /// input_ref が違えば hash も違う（同一キー + 異なる退避入力を弁別する）。
    #[test]
    fn request_hash_distinguishes_input_ref() {
        let null = Value::Null;
        assert_ne!(
            invoke_request_hash("echo", &null, Some("tenants/t/io/exec_a/input")),
            invoke_request_hash("echo", &null, Some("tenants/t/io/exec_b/input"))
        );
        // input_ref 有無も弁別する。
        assert_ne!(
            invoke_request_hash("echo", &null, Some("tenants/t/io/exec_a/input")),
            invoke_request_hash("echo", &null, None)
        );
    }

    /// sha256 hex 形（64 文字の小文字 16進）。
    #[test]
    fn request_hash_is_sha256_hex() {
        let h = invoke_request_hash("echo", &json!({}), None);
        assert_eq!(h.len(), 64);
        assert!(h
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    /// 配列の順序は意味を持つ（順序が違えば hash が違う）。
    #[test]
    fn request_hash_array_order_matters() {
        assert_ne!(
            invoke_request_hash("echo", &json!([1, 2, 3]), None),
            invoke_request_hash("echo", &json!([3, 2, 1]), None)
        );
    }

    // ---- M3d 大入力: input_ref 完全一致検証 (§3.4) -----------------------------

    /// 当該テナント・当該 execution の入力キーへ完全一致すれば受理する。
    #[test]
    fn input_ref_exact_match_accepts() {
        // shared の io_input_key と同一レイアウトに完全一致する値だけ受理。
        let key = faas_shared::io_input_key("ten_a", "exec_1");
        assert!(validate_input_ref(&key, "ten_a", "exec_1").is_ok());
    }

    /// prefix 一致は拒否する（完全一致のみ, §3.4 MUST）。
    #[test]
    fn input_ref_rejects_prefix() {
        // 末尾を削った prefix。
        assert!(matches!(
            validate_input_ref("tenants/ten_a/io/exec_1", "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
        // ディレクトリ prefix（input より上位）。
        assert!(matches!(
            validate_input_ref("tenants/ten_a/io/exec_1/", "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
        // 別キー（input ではなく output）。
        assert!(matches!(
            validate_input_ref("tenants/ten_a/io/exec_1/output", "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
        // 末尾に余剰パスを付けた値（prefix としては一致するが完全一致ではない）。
        assert!(matches!(
            validate_input_ref("tenants/ten_a/io/exec_1/input/extra", "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
    }

    /// cross-exec を拒否する（同一テナント内の別 execution の入力を指す input_ref）。
    #[test]
    fn input_ref_rejects_cross_execution() {
        // 予約 execution は exec_1 だが input_ref は exec_2 を指す。
        let other = faas_shared::io_input_key("ten_a", "exec_2");
        assert!(matches!(
            validate_input_ref(&other, "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
    }

    /// cross-tenant を拒否する（別テナントの入力を指す input_ref）。
    #[test]
    fn input_ref_rejects_cross_tenant() {
        // caller テナントは ten_a だが input_ref は ten_b を指す。
        let other = faas_shared::io_input_key("ten_b", "exec_1");
        assert!(matches!(
            validate_input_ref(&other, "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
        // テナントの substring 混同（ten_a vs ten_a2）も完全一致で弾く。
        let confusable = faas_shared::io_input_key("ten_a2", "exec_1");
        assert!(matches!(
            validate_input_ref(&confusable, "ten_a", "exec_1"),
            Err(FaasError::InvalidRequest(_))
        ));
    }

    /// 予約 execution_id の形式検証: exec_ プレフィックス + key-safe charset。
    #[test]
    fn reserved_execution_id_format() {
        assert!(validate_reserved_execution_id("exec_abc123").is_ok());
        assert!(validate_reserved_execution_id("exec_a-b_c").is_ok());
        // 非 exec_ プレフィックス。
        assert!(validate_reserved_execution_id("ten_abc").is_err());
        // パス traversal / 区切り文字（キー空間に埋め込むため不可）。
        assert!(validate_reserved_execution_id("exec_../../etc").is_err());
        assert!(validate_reserved_execution_id("exec_a/b").is_err());
        assert!(validate_reserved_execution_id("exec_a b").is_err());
    }

    /// 正準化は決定的（同じ入力で同じバイト列）。
    #[test]
    fn canonical_json_bytes_deterministic() {
        let v = json!({"z": [1, {"b": 2, "a": 1}], "a": "x"});
        assert_eq!(canonical_json_bytes(&v), canonical_json_bytes(&v));
    }

    /// is_unique_violation は 23505 のみ true。
    #[test]
    fn unique_violation_detection() {
        // RowNotFound は unique violation ではない。
        assert!(!is_unique_violation(&sqlx::Error::RowNotFound));
    }

    // ---- M4a (§3.8) /readyz: 503 fail-closed と Prometheus exposition ---------
    //
    // ハンドラ本体は AppState（PgPool + NATS + Store）を要求するため、テストは
    // DB / NATS の readiness 判定を担う [`check_db_ready`] と最終応答ビルダ
    // [`build_readyz_response`] に対して個別に行う。これにより「DB ping 失敗」だけを
    // 確実に独立検証できる（NATS / Store を本物で起動しなくて済む）。

    /// `build_readyz_response`: 全 hop が Ok のとき 200 + ボディに "ok"。
    #[tokio::test]
    async fn readyz_returns_200_when_all_hops_ok() {
        use axum::body::to_bytes;
        let resp = build_readyz_response(Ok(()), Ok(()), Ok(()));
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["db"], "ok");
        assert_eq!(v["nats"], "ok");
        assert_eq!(v["store"], "ok");
    }

    /// `build_readyz_response`: いずれか 1 hop が Err なら 503 にフェイル（fail-closed）。
    /// 残りの hop の "ok" 判定はそのまま JSON ボディに残す（運用者が原因 hop を判別できる）。
    #[tokio::test]
    async fn readyz_returns_503_when_db_check_fails() {
        use axum::body::to_bytes;
        let resp = build_readyz_response(Err("db: connection closed".to_string()), Ok(()), Ok(()));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v["db"]
                .as_str()
                .map(|s| s.starts_with("db:"))
                .unwrap_or(false),
            "db hop should report its error message: got {:?}",
            v["db"]
        );
        // 他の hop の判定はそのまま残す（503 でも per-hop 状態を運用者に見せる）。
        assert_eq!(v["nats"], "ok");
        assert_eq!(v["store"], "ok");
    }

    /// `build_readyz_response`: NATS / Store それぞれの障害でも 503（fail-closed）。
    #[tokio::test]
    async fn readyz_returns_503_when_nats_or_store_fails() {
        let resp = build_readyz_response(Ok(()), Err("nats: down".to_string()), Ok(()));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = build_readyz_response(Ok(()), Ok(()), Err("store: down".to_string()));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // 2 つ以上が落ちても変わらず 503。
        let resp = build_readyz_response(
            Err("db: x".to_string()),
            Err("nats: y".to_string()),
            Err("store: z".to_string()),
        );
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// `check_db_ready`: 閉じた / 到達不能な PgPool に対しては `Err` を返す（hang しない）。
    ///
    /// `connect_lazy` で実接続を作らない pool に対して `SELECT 1` を投げると即 fail
    /// する（unreachable host + 短いタイムアウト）。これにより「DB が落ちたとき /readyz は
    /// 503 を返す」スライス完了条件が live DB 無しで検証できる。
    #[tokio::test]
    async fn check_db_ready_errors_when_pool_unreachable() {
        // unreachable な URL（接続不可ポート）。connect_lazy なので構築自体は成功する。
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("postgres://faas:faas@127.0.0.1:1/faas")
            .expect("connect_lazy never fails on parse-valid URLs");
        let res = check_db_ready(&pool).await;
        assert!(
            res.is_err(),
            "check_db_ready against an unreachable pool must return Err; got {res:?}"
        );
    }

    // ---- M4a (§3.8) /metrics: Prometheus exposition の最低限の構造 -----------
    //
    // 専用パーサ（prometheus-parse）に依存せず、Prometheus text format の不変条件:
    // HELP / TYPE 行、サンプル行、改行終端、Content-Type をテストする。
    // exposition の形が壊れると、後段の Prometheus / Grafana が静かに読み飛ばすため
    // 退行ガードとして必要。

    /// `Metrics::render` は `# HELP` / `# TYPE` 行とサンプル行を持つ Prometheus 形式を返す。
    #[test]
    fn metrics_render_produces_parseable_exposition() {
        let m = crate::metrics::Metrics::init();
        // 既定 0 のままだとサンプル行が出ないメトリクスがあるため、各種を 1 度 inc しておく。
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.executions_total.with_label_values(&["failed"]).inc();
        m.admission_rejections_total
            .with_label_values(&["rate_limited"])
            .inc();
        m.dlq_finalized_total
            .with_label_values(&["finalized"])
            .inc();
        m.tenant_invoke_total.with_label_values(&["ten_a"]).inc();

        let (headers, body) = m.render();
        // Content-Type は prometheus 標準（"text/plain; version=0.0.4"）。
        let ct = headers
            .get(axum::http::header::CONTENT_TYPE)
            .expect("Content-Type must be set");
        let ct = ct.to_str().expect("ascii");
        assert!(
            ct.starts_with("text/plain"),
            "Prometheus exposition must be text/plain; got {ct:?}"
        );

        // HELP / TYPE / サンプル行が全て揃っている。
        assert!(body.contains("# HELP faas_executions_total"));
        assert!(body.contains("# TYPE faas_executions_total counter"));
        assert!(body.contains("faas_executions_total{status=\"succeeded\"} 1"));
        assert!(body.contains("faas_executions_total{status=\"failed\"} 1"));

        // DLQ 経路の outcome ラベル付きサンプル（M4c）。
        assert!(body.contains("# HELP faas_dlq_finalized_total"));
        assert!(body.contains("faas_dlq_finalized_total{outcome=\"finalized\"} 1"));

        // admission 429 の kind ラベル付きサンプル（M4d）。
        assert!(body.contains("faas_admission_rejections_total{kind=\"rate_limited\"} 1"));

        // テナント別 invoke カウンタ（M4a per-tenant 観測）。
        assert!(body.contains("faas_tenant_invoke_total{tenant_id=\"ten_a\"} 1"));

        // 形式の最低保証: 各行が `\n` で終わり、空でない・"# HELP" の数 == "# TYPE" の数。
        assert!(
            body.ends_with('\n'),
            "Prometheus exposition must end with newline"
        );
        let help_lines = body.lines().filter(|l| l.starts_with("# HELP ")).count();
        let type_lines = body.lines().filter(|l| l.starts_with("# TYPE ")).count();
        assert_eq!(
            help_lines, type_lines,
            "every metric must have matching HELP and TYPE lines"
        );
        assert!(
            help_lines > 0,
            "exposition must contain at least one metric"
        );
    }
}
