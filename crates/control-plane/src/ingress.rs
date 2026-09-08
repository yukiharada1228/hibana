//! 公開 HTTP ingress gateway。
//!
//! `<app>.<tenant>.<base>` の Host からテナント/アプリを解決し、**明示的に
//! `ingress_enabled` を立てた** component だけを実行して HTTP 応答を返す。
//! deny-by-default（未 opt-in / 版なし / 不明 host は存在を晒さず 404）。
//!
//! 本番構成では管理 API と分けたアプリ用 listener の fallback として動く。
//! Host が ingress ベースドメイン配下のものだけを処理し、それ以外は 404。
//!
//! 外向き通信の制御は Worker の runtime が担当する。ここではテナントを解決し、
//! admission / 受付記録 / 署名付き直接HTTP転送を行う。

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use hibana_shared::http::{HttpRequest, MAX_REQUEST_BYTES};

use crate::state::AppState;

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Host（`app.tenant.base[:port]`, 小文字化済み）から `(app, tenant)` を取り出す。
/// ちょうど `app.tenant.<base>` の形（app / tenant は各 1 ラベル）でなければ `None`。
fn split_host<'a>(host: &'a str, base: &str) -> Option<(&'a str, &'a str)> {
    let host = host.split(':').next().unwrap_or(host);
    let rest = host.strip_suffix(base)?.strip_suffix('.')?; // "app.tenant"
    let (app, tenant) = rest.rsplit_once('.')?;
    // app / tenant は単一ラベル（余分な '.' はサブドメイン多段 = 非対応 → 404）。
    if app.is_empty() || tenant.is_empty() || app.contains('.') {
        return None;
    }
    Some((app, tenant))
}

/// axum fallback: ingress host なら gateway 処理、それ以外は 404。
pub async fn ingress_fallback(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let Some(base) = state.ingress_base_domain() else {
        // gateway 無効（INGRESS_BASE_DOMAIN 未設定）。
        return not_found();
    };
    let base = base.to_string();

    let (parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some((app, tenant_slug)) = split_host(&host, &base) else {
        return not_found();
    };

    // 1) tenant_slug -> tenant_id（GUC 不要の SECURITY DEFINER 経路。login と同じ）。
    let tenant_id = match db_find_tenant(&state, tenant_slug).await {
        Some(t) => t,
        None => return not_found(),
    };

    // 2) component 解決 + deny-by-default gate（GUC 下）。
    let comp = match db_find_component(&state, &tenant_id, app).await {
        Some(c) => c,
        None => return not_found(),
    };
    if !comp.ingress_enabled || comp.active_version_id.is_none() {
        // 未 opt-in / active 版なしは「存在しない」と同じ扱い（情報を晒さない）。
        return not_found();
    }

    // 3) 実リクエスト -> HTTP エンベロープ JSON。
    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    let envelope = serde_json::to_value(HttpRequest::from_parts(&parts, &body_bytes))
        .expect("HTTP request serialization");

    match crate::direct_http::accept(&state, &tenant_id, app, envelope).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

// --- DB ラッパ（GUC 境界をここに閉じる） ------------------------------------

async fn db_find_tenant(state: &AppState, slug: &str) -> Option<String> {
    crate::db::find_tenant_id_by_slug(state.pool(), slug)
        .await
        .ok()
        .flatten()
}

async fn db_find_component(
    state: &AppState,
    tenant_id: &str,
    name: &str,
) -> Option<crate::db::IngressComponentRow> {
    let mut tx = state.pool().begin().await.ok()?;
    if crate::db::set_tenant_guc(&mut tx, tenant_id).await.is_err() {
        return None;
    }
    let row = crate::db::find_ingress_component(&mut *tx, tenant_id, name)
        .await
        .ok()
        .flatten();
    let _ = tx.commit().await;
    row
}

#[cfg(test)]
mod tests {
    use super::split_host;

    #[test]
    fn splits_app_tenant_base() {
        assert_eq!(
            split_host("my-api.smoke.hibana.local", "hibana.local"),
            Some(("my-api", "smoke"))
        );
    }

    #[test]
    fn strips_port() {
        assert_eq!(
            split_host("my-api.smoke.hibana.local:8080", "hibana.local"),
            Some(("my-api", "smoke"))
        );
    }

    #[test]
    fn rejects_wrong_base() {
        assert_eq!(split_host("my-api.smoke.evil.com", "hibana.local"), None);
    }

    #[test]
    fn rejects_bare_base() {
        assert_eq!(split_host("hibana.local", "hibana.local"), None);
    }

    #[test]
    fn rejects_extra_labels() {
        // app が多段（a.b.smoke）は非対応。
        assert_eq!(split_host("a.b.smoke.hibana.local", "hibana.local"), None);
    }

    #[test]
    fn rejects_missing_tenant() {
        assert_eq!(split_host("app.hibana.local", "hibana.local"), None);
    }
}
