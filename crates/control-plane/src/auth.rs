//! 認証・認可層 (M3a: API トークン + テナント解決)。
//!
//! CLIのBearerトークン、またはコンソールのHttpOnly Cookieから取り出した
//! `secret` を sha256(hex) でハッシュし、
//! `api_tokens.token_hash` と突き合わせて principal を確立する (§3.3 / §6.0)。
//! ハッシュ照合のため、平文比較のような非定数時間比較は使わない（DB の UNIQUE
//! インデックス照合に委ねる）。
//!
//! - `authenticate`: 全保護ルートに適用する middleware。`Principal` を request
//!   extension に挿入する。`/healthz` `/auth/oidc/*` `POST /admin/tenants` には
//!   適用しない（ルータ側で分離）。
//! - `Principal`: 認証済み呼び出し主体。`FromRequestParts` で各ハンドラへ注入する。
//! - `require_scope`: スコープ不足を 403 にする route layer。
use sea_orm::TransactionTrait as _;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hibana_shared::{FaasError, Role, Scope};

use crate::db;
use crate::error::AppError;
use crate::state::AppState;

/// 認証済みの呼び出し主体（principal）。request extension 経由でハンドラへ渡る。
///
/// テナント境界、スコープ・ロール認可、監査ログの主体を保持する。
#[derive(Debug, Clone)]
pub struct Principal {
    /// 解決済みテナント ID（全 DB アクセスのテナント境界）。
    pub tenant_id: String,
    /// トークンに紐づくユーザ ID（サービストークンは `None`）。
    pub user_id: Option<String>,
    /// 認証に使われた API トークン ID。
    pub token_id: String,
    /// トークンに付与されたスコープ集合。
    pub scopes: Vec<Scope>,
    /// トークン所有ユーザのロール（サービストークンは `Admin` 扱い: ユーザ無し
    /// なのでロール上限は scopes でのみ制約される）。
    pub role: Role,
}

impl Principal {
    /// 指定スコープを保持するか。
    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    /// audit_logs の `actor` に載せる安定識別子（§3.7）。
    /// ユーザ紐づきトークンは user_id、サービストークンは認証に使った token_id を返す。
    pub fn actor(&self) -> Option<&str> {
        self.user_id.as_deref().or(Some(self.token_id.as_str()))
    }
}

/// `Authorization: Bearer` から平文 secret を取り出す。
fn extract_bearer(req_headers: &axum::http::HeaderMap) -> Option<String> {
    req_headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 提示された平文 secret を sha256 でハッシュし、hex 文字列を返す。
///
/// DB の `api_tokens.token_hash` と同一形式（小文字 hex）。
pub fn hash_token(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    hex::encode(digest)
}

/// 認証 middleware。Bearer/Cookieのsecretをハッシュ照合し `Principal` を確立する。
///
/// 失敗（欠損・不一致・失効・期限切れ）は一律 `401 Unauthorized`。成功時は
/// `Principal` を request extension に挿入して次へ進む。
///
/// M4d (§3.2 / §8): principal 確立後、テナントの `status` を 1 query で引き、`suspended` なら
/// 自身のセッション確認・失効を除き、read/admin 系を含む保護ルートを 403 にする。
/// invoke だけで弾くと「停止されたテナントがトークンを使い読み取り・管理操作を継続できる」状態に
/// なるため、middleware-level でまとめて拒否する方が一貫性が高い。403 は best-effort で audit_logs に
/// 残し、停止テナントが API キーで叩いてきた事実を運用に可視化する（停止された側がブルートフォース等
/// 試みていないかの観測）。tenants に RLS は無いので GUC 無しで読める。
pub async fn authenticate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let clear_cookie = !req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION)
        && matches!(req.uri().path(), "/auth/logout" | "/auth/logout-all");
    let config = state.auth_config().clone();
    let mut response = match authenticate_request(state, req, next).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    if clear_cookie && response.status().is_success() {
        response.headers_mut().insert(
            axum::http::header::SET_COOKIE,
            crate::oidc::session::cookie(&config, "", 0),
        );
    }
    crate::oidc::no_store(response)
}

#[derive(Clone)]
pub struct TokenMetadata {
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

async fn authenticate_request(
    state: AppState,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let browser = !req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION);
    let secret = if browser {
        let secret = crate::oidc::session::credential(state.auth_config(), req.headers())?
            .ok_or(FaasError::Unauthorized)?;
        crate::oidc::session::require_console(state.auth_config(), req.headers(), req.method())?;
        secret
    } else {
        extract_bearer(req.headers()).ok_or(FaasError::Unauthorized)?
    };
    let token_hash = hash_token(&secret);

    let row = db::find_token_by_hash(state.pool(), &token_hash)
        .await?
        .ok_or(FaasError::Unauthorized)?;

    if browser
        && (row.auth_method != "oidc"
            || !crate::oidc::session::matches_session(
                req.headers(),
                req.method(),
                req.uri().path(),
                &row.token_id,
            ))
    {
        return Err(FaasError::Unauthorized.into());
    }
    req.extensions_mut().insert(TokenMetadata {
        expires_at: row.expires_at,
    });

    let principal =
        principal_from_token(state.auth_config(), row).ok_or(FaasError::Unauthorized)?;

    // M4d: テナント status / quotas を 1 row で取得する（ホットパス sub-ms PK lookup）。
    // status='suspended' は 403 で短絡。`tenant_not_found` は理論上 principal 確立後には
    // 起こりえない（FK 制約 + cascade）ため 401 ではなく 500（Internal）扱い: principal の
    // 整合性が壊れているシグナル。
    match db::load_tenant_status_and_quotas(state.pool(), &principal.tenant_id).await? {
        Some((status, _quotas))
            if status == "suspended" && !self_session_route(req.method(), req.uri().path()) =>
        {
            // 監査追記（best-effort: 失敗しても 403 応答は返す）。
            audit_tenant_suspended_denied(&state, &principal).await;
            return Err(FaasError::Forbidden.into());
        }
        Some(_) => {
            // active、または停止中でも許可する自身のセッション操作。
        }
        None => {
            // principal は確立済みなのに tenants に行が無い = データ整合性の異常。
            tracing::error!(
                tenant = %principal.tenant_id,
                "authenticated principal references a non-existent tenant"
            );
            return Err(FaasError::Internal(String::new()).into());
        }
    }

    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

// Suspension forbids tenant operations, but must not trap a browser in a valid
// HttpOnly session. These routes expose only the caller's identity or revoke it.
fn self_session_route(method: &axum::http::Method, path: &str) -> bool {
    match path {
        "/auth/session" => matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD),
        "/auth/logout" | "/auth/logout-all" => *method == axum::http::Method::POST,
        _ => false,
    }
}

/// Apply the same authentication rules both at admission and when a locked
/// token is rechecked inside a credential-issuing transaction.
pub(crate) fn principal_from_token(
    config: &crate::oidc::config::OidcConfig,
    row: db::TokenRow,
) -> Option<Principal> {
    match row.auth_method.as_str() {
        "api" => {}
        "oidc" if row.user_oidc_issuer.as_deref() == Some(config.issuer.as_str()) => {}
        _ => return None,
    }
    row.into_principal()
}

/// M4d (§3.7): tenant_suspended による拒否を audit_logs に記録する（best-effort）。
///
/// 停止されたテナントが API キーで叩いてきた事実を残すため。tenants は RLS の対象外だが、
/// audit_logs は FORCE RLS 下にあるため tx 冒頭で `app.tenant_id` GUC を **当該テナント**
/// （= principal.tenant_id）にセットする（停止中であっても tenant_id 自体は権威値）。
async fn audit_tenant_suspended_denied(state: &AppState, principal: &Principal) {
    use serde_json::json;
    let detail = json!({ "reason": "tenant_suspended", "token_id": principal.token_id });
    let res: anyhow::Result<()> = async {
        let tx = state.pool().begin().await?;
        db::set_tenant_guc(&tx, &principal.tenant_id).await?;
        db::insert_audit_log(
            &tx,
            &principal.tenant_id,
            principal.actor(),
            "tenant_suspended_denied",
            None,
            Some(&detail),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(
            tenant = %principal.tenant_id,
            error = %e,
            "failed to write tenant_suspended_denied audit row"
        );
    }
}

impl<S> FromRequestParts<S> for Principal
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| FaasError::Unauthorized.into())
    }
}

/// 指定スコープが必須のルートに被せる route layer を返す。
///
/// `Principal` を extension から読み、当該スコープが無ければ `403 Forbidden`。
/// `authenticate` が先に走って extension を挿入している前提（不在は 401）。
pub fn require_scope(
    scope: Scope,
) -> impl Clone
       + Send
       + Sync
       + 'static
       + Fn(
    Request,
    Next,
)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, AppError>> + Send>> {
    move |req: Request, next: Next| {
        Box::pin(async move {
            let principal = req
                .extensions()
                .get::<Principal>()
                .cloned()
                .ok_or(FaasError::Unauthorized)?;
            if !principal.has_scope(scope) {
                return Err(FaasError::Forbidden.into());
            }
            Ok(next.run(req).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspension_exceptions_only_manage_the_callers_session() {
        use axum::http::Method;
        assert!(self_session_route(&Method::GET, "/auth/session"));
        assert!(self_session_route(&Method::POST, "/auth/logout"));
        assert!(self_session_route(&Method::POST, "/auth/logout-all"));
        for (method, path) in [
            (Method::POST, "/auth/session"),
            (Method::GET, "/auth/logout"),
            (Method::POST, "/auth/oidc/exchange"),
            (Method::POST, "/tokens"),
            (Method::GET, "/users"),
            (Method::DELETE, "/users/another"),
            (Method::GET, "/components"),
        ] {
            assert!(!self_session_route(&method, path));
        }
    }

    #[test]
    fn hash_token_is_lowercase_hex_sha256() {
        // sha256("") は既知の定数。
        let h = hash_token("");
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_token_is_deterministic_and_distinct() {
        assert_eq!(hash_token("secret-abc"), hash_token("secret-abc"));
        assert_ne!(hash_token("secret-abc"), hash_token("secret-abd"));
    }
}
