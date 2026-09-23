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
use sea_orm::TransactionTrait as _;
pub(crate) mod body;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use hibana_shared::http::HttpRequest;

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
        // gateway 無効（APP_PUBLIC_ORIGIN 未設定）。
        return not_found();
    };
    let (parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some((app, tenant_slug)) = split_host(&host, base) else {
        return not_found();
    };

    // Bound DB lookup, body buffering and dispatch together. A slow sender must
    // acquire capacity before allocating a body or queuing identity queries.
    let Some(_request_slot) = state.request_capacity().reserve() else {
        return crate::admission::RateLimited::request_capacity().into_response();
    };

    // 1) tenant_slug -> tenant_id（GUC 不要の SECURITY DEFINER 経路。login と同じ）。
    let tenant_id = match crate::db::find_tenant_id_by_slug(state.pool(), tenant_slug).await {
        Ok(Some(t)) => t,
        Ok(None) => return not_found(),
        Err(error) => return lookup_unavailable(error),
    };

    // 2) component 解決 + deny-by-default gate（GUC 下）。
    let comp = match db_find_component(&state, &tenant_id, app).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(),
        Err(error) => return lookup_unavailable(error),
    };
    if !comp.ingress_enabled || comp.active_version_id.is_none() {
        // 未 opt-in / active 版なしは「存在しない」と同じ扱い（情報を晒さない）。
        return not_found();
    }

    // The platform origin is authoritative behind TLS termination. Never trust
    // client-supplied Forwarded/X-Forwarded-* headers or an arbitrary Host port.
    let Some(origin) = state
        .app_url(app, tenant_slug)
        .and_then(|url| url.parse::<axum::http::Uri>().ok())
    else {
        return not_found();
    };
    // The future is polled only after tenant status, rate and receive capacity
    // checks. Keep the global permit until the envelope has left this process.
    let input = async move {
        let body_bytes = body::read(body).await.map_err(|response| *response)?;
        let mut request = HttpRequest::from_parts(&parts, &body_bytes);
        request.scheme = origin
            .scheme_str()
            .expect("configured public scheme")
            .into();
        request.authority = origin
            .authority()
            .expect("configured public authority")
            .to_string();
        request.headers.insert(
            "host".into(),
            vec![hibana_shared::b64url_encode(request.authority.as_bytes())],
        );
        Ok(serde_json::to_value(request).expect("HTTP request serialization"))
    };

    match crate::direct_http::accept(&state, &tenant_id, app, input).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

// --- DB ラッパ（GUC 境界をここに閉じる） ------------------------------------

fn lookup_unavailable(error: sea_orm::DbErr) -> Response {
    tracing::error!(error = %error, "ingress application lookup failed");
    crate::error::AppError(hibana_shared::FaasError::Unavailable).into_response()
}

async fn db_find_component(
    state: &AppState,
    tenant_id: &str,
    name: &str,
) -> Result<Option<crate::db::IngressComponentRow>, sea_orm::DbErr> {
    let tx = state.pool().begin().await?;
    crate::db::set_tenant_guc(&tx, tenant_id).await?;
    let row = crate::db::find_ingress_component(&tx, tenant_id, name).await?;
    tx.commit().await?;
    Ok(row)
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
