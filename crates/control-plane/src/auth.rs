//! 認証・認可層 (M3a: API トークン + テナント解決)。
//!
//! `Authorization: Bearer ${secret}` の `secret` を sha256(hex) でハッシュし、
//! `api_tokens.token_hash` と突き合わせて principal を確立する (§3.3 / §6.0)。
//! ハッシュ照合のため、平文比較のような非定数時間比較は使わない（DB の UNIQUE
//! インデックス照合に委ねる）。
//!
//! - `authenticate`: 全保護ルートに適用する middleware。`Principal` を request
//!   extension に挿入する。`/healthz` `/auth/login` `POST /admin/tenants` には
//!   適用しない（ルータ側で分離）。
//! - `Principal`: 認証済み呼び出し主体。`FromRequestParts` で各ハンドラへ注入する。
//! - `require_scope`: スコープ不足を 403 にする route layer。

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use hibana_shared::{FaasError, Role, Scope};

use crate::db;
use crate::error::AppError;
use crate::state::AppState;

/// 認証済みの呼び出し主体（principal）。request extension 経由でハンドラへ渡る。
///
/// `user_id` / `token_id` / `role` は principal 契約（§6.0）の一部として確立する。
/// M3a の現ハンドラ群は `tenant_id` と `scopes` のみ参照するが、後続スライス
/// （監査ログ・per-user 認可・トークン所有確認）で読むため保持する。
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    hex_lower(&digest)
}

/// バイト列を小文字 hex 文字列に変換する。
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// 認証 middleware。Bearer secret をハッシュ照合し `Principal` を確立する。
///
/// 失敗（欠損・不一致・失効・期限切れ）は一律 `401 Unauthorized`。成功時は
/// `Principal` を request extension に挿入して次へ進む。
///
/// M4d (§3.2 / §8): principal 確立後、テナントの `status` を 1 query で引き、`suspended` なら
/// **全 API パスを 403 で短絡**する（invoke だけでなく read/admin 系も含む全保護ルートに適用される）。
/// invoke だけで弾くと「停止されたテナントがトークンを使い読み取り・管理操作を継続できる」状態に
/// なるため、middleware-level でまとめて拒否する方が一貫性が高い。403 は best-effort で audit_logs に
/// 残し、停止テナントが API キーで叩いてきた事実を運用に可視化する（停止された側がブルートフォース等
/// 試みていないかの観測）。tenants に RLS は無いので GUC 無しで読める。
pub async fn authenticate(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let secret = extract_bearer(req.headers()).ok_or(FaasError::Unauthorized)?;
    let token_hash = hash_token(&secret);

    let row = db::find_token_by_hash(state.pool(), &token_hash)
        .await?
        .ok_or(FaasError::Unauthorized)?;

    let principal = row.into_principal().ok_or(FaasError::Unauthorized)?;

    // M4d: テナント status / quotas を 1 row で取得する（ホットパス sub-ms PK lookup）。
    // status='suspended' は 403 で短絡。`tenant_not_found` は理論上 principal 確立後には
    // 起こりえない（FK 制約 + cascade）ため 401 ではなく 500（Internal）扱い: principal の
    // 整合性が壊れているシグナル。
    match db::load_tenant_status_and_quotas(state.pool(), &principal.tenant_id).await? {
        Some((status, _quotas)) if status == "suspended" => {
            // 監査追記（best-effort: 失敗しても 403 応答は返す）。
            audit_tenant_suspended_denied(&state, &principal).await;
            return Err(FaasError::Forbidden.into());
        }
        Some(_) => { /* active: 続行 */ }
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

/// M4d (§3.7): tenant_suspended による拒否を audit_logs に記録する（best-effort）。
///
/// 停止されたテナントが API キーで叩いてきた事実を残すため。tenants は RLS の対象外だが、
/// audit_logs は FORCE RLS 下にあるため tx 冒頭で `app.tenant_id` GUC を **当該テナント**
/// （= principal.tenant_id）にセットする（停止中であっても tenant_id 自体は権威値）。
async fn audit_tenant_suspended_denied(state: &AppState, principal: &Principal) {
    use serde_json::json;
    let detail = json!({ "reason": "tenant_suspended", "token_id": principal.token_id });
    let res: anyhow::Result<()> = async {
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, &principal.tenant_id).await?;
        db::insert_audit_log(
            &mut *tx,
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
