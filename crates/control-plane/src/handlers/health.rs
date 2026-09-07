//! Health management HTTP handlers.
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

// ---------------------------------------------------------------------------
// GET /healthz / /readyz / /metrics  (M4a, §3.8)
// ---------------------------------------------------------------------------

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    build_readyz_response(
        check_db_ready(state.pool()).await,
        check_store_ready(state.store()).await,
    )
}

/// M4a (§3.8): DB の readiness ping（`SELECT 1` を 500ms タイムアウト付きで一度だけ）。
///
/// 別関数に切り出すのは、ユニットテストで「接続できない / 閉じたプール」に対して 503 系の
/// `Err` が返ることを直接検証するため。ハンドラ本体（[`readyz`]）は AppState を要求するため
/// テスト時のセットアップが重く、肝心の DB-down 経路を覆えなくなる。
pub(super) async fn check_db_ready(pool: &sqlx::PgPool) -> Result<(), String> {
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

/// Store の readiness 判定（`ping()` を直接呼ぶ）。
pub(super) async fn check_store_ready(store: &dyn crate::store::Store) -> Result<(), String> {
    match store.ping().await {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("store: {e}")),
    }
}

/// per-hop の判定結果から 200 or 503 レスポンスを構築する（M4a, §3.8）。
///
/// DBまたは共有ストアの異常は503。
pub(super) fn build_readyz_response(db: Result<(), String>, store: Result<(), String>) -> Response {
    let body = json!({
        "db": match &db { Ok(()) => json!("ok"), Err(e) => json!(e) },
        "store": match &store { Ok(()) => json!("ok"), Err(e) => json!(e) },
    });

    let all_ok = db.is_ok() && store.is_ok();
    if all_ok {
        (StatusCode::OK, Json(body)).into_response()
    } else {
        // 失敗詳細はログにも残す（503 のたびに stderr に出して観測しやすくする）。
        tracing::warn!(?db, ?store, "readyz: not ready");
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
