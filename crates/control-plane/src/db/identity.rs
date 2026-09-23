//! Identity persistence.
use crate::auth::Principal;
use chrono::DateTime;
use chrono::Utc;
use hibana_database::prelude::*;
use hibana_shared::Role;

pub async fn session_tenant(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<Option<(String, String)>, DbErr> {
    tenants::Entity::find_by_id(tenant_id)
        .select_only()
        .columns([tenants::Column::Slug, tenants::Column::Name])
        .into_tuple()
        .one(executor)
        .await
}

// ---------------------------------------------------------------------------
// 認証・認可: users / api_tokens (§3.3 / §6.0)
// ---------------------------------------------------------------------------

/// `api_tokens` の照合結果行（認証ホットパス）。
///
/// 失効/期限の判定は `into_principal`（純関数化のため `is_token_valid` に委譲）で行う。
#[derive(Debug, Clone, FromQueryResult)]
pub struct TokenRow {
    #[sea_orm(from_alias = "id")]
    pub token_id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// 紐づくユーザのロール。サービストークン（user_id NULL）は `None`。
    pub user_role: Option<String>,
    pub auth_method: String,
    pub user_oidc_issuer: Option<String>,
}

/// トークンが現時点で有効か（失効しておらず、かつ未期限切れ）。
///
/// 純関数（DB 不要）でテスト可能にしておく。`now` を引数に取り、時計依存を排除する。
pub fn is_token_valid(
    revoked_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    revoked_at.is_none() && expires_at > now
}

/// 文字列ロールを `Role` へパースする。未知/欠損は `None`。
pub fn parse_role(raw: Option<&str>) -> Option<Role> {
    match raw {
        Some("member") => Some(Role::Member),
        Some("admin") => Some(Role::Admin),
        _ => None,
    }
}

impl TokenRow {
    /// 有効なトークンなら `Principal` を構築する。失効/期限切れは `None`。
    ///
    /// サービストークン（user_id NULL）はユーザロールが無いため `Role::Admin`
    /// 扱いとする（ロール上限ではなく付与済み scopes でのみ権限が制約される）。
    pub fn into_principal(self) -> Option<Principal> {
        self.into_principal_at(Utc::now())
    }

    /// `now` を明示する版（テスト用）。
    pub fn into_principal_at(self, now: DateTime<Utc>) -> Option<Principal> {
        if !is_token_valid(self.revoked_at, self.expires_at, now) {
            return None;
        }
        let role = match (self.user_id.as_ref(), self.user_role.as_deref()) {
            (None, None) => Role::Admin,
            (Some(_), Some(role)) => parse_role(Some(role))?,
            _ => return None,
        };
        // Resolve only scopes allowed by the role, using their shared DB names.
        // Unknown scopes are discarded along with scopes above the role ceiling.
        let scopes = self
            .scopes
            .iter()
            .filter_map(|raw| {
                role.ceiling()
                    .iter()
                    .copied()
                    .find(|scope| scope.as_str() == raw)
            })
            .collect();
        Some(Principal {
            tenant_id: self.tenant_id,
            user_id: self.user_id,
            token_id: self.token_id,
            scopes,
            role,
        })
    }
}

/// token_hash で API トークンを照合する（認証ホットパス, §3.3 / §15 M3b）。
///
/// 平文比較ではなく UNIQUE インデックス照合に委ねる（非定数時間比較を排除）。
/// 失効/期限切れの判定は呼び出し側（`TokenRow::into_principal`）で行う。
///
/// M3b: これは**テナントコンテキスト確立前**に走る（戻り値の tenant_id がこの後の
/// リクエストで GUC を設定する）。よって FORCE RLS 下では GUC 未設定で api_tokens を
/// 読めない。SECURITY DEFINER 関数 `auth_lookup_token_by_hash`（所有者権限で実行され
/// RLS 対象外）を呼ぶことで、GUC 無しの認証前参照を成立させる（migrations/src/m20260915_000001_security.sql）。
pub async fn find_token_by_hash(
    executor: &impl ConnectionTrait,
    token_hash: &str,
) -> Result<Option<TokenRow>, DbErr> {
    function_rows(
        executor,
        "auth_lookup_token_by_hash",
        vec![token_hash.into()],
    )
    .await?
    .first()
    .map(|row| TokenRow::from_query_result(row, ""))
    .transpose()
}

/// テナントを作成する（bootstrap: POST /admin/tenants）。slug は UNIQUE。
/// テナントと最初の admin ユーザを**1 トランザクション**で作成する（§9 bootstrap）。
///
/// `POST /admin/tenants` の chicken-and-egg を解消する: 通常の `POST
/// /tenants/{id}/users` は Admin トークンを要するが、最初のユーザはまだトークンが
/// 無いため作れない。bootstrap トークンで保護されたこの経路だけが、テナントと
/// 最初の admin ユーザを同時に作る。失敗時は両方ロールバックされる。
///
/// 呼び出し側が所有する同一トランザクションで、先に `set_tenant_guc` を呼ぶこと。
/// users は FORCE RLS 下で WITH CHECK が GUC と一致する必要がある。
/// 最初の管理者も、作成時から OIDC identity を持つ。
#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_tenant(
    conn: &impl ConnectionTrait,
    tenant_id: &str,
    slug: &str,
    name: &str,
    admin_user_id: &str,
    admin_email: &str,
    issuer: &str,
    subject: &str,
) -> Result<(), DbErr> {
    tenants::Entity::insert(tenants::ActiveModel {
        id: Set(tenant_id.into()),
        slug: Set(slug.into()),
        name: Set(name.into()),
        status: Set("active".into()),
        ..Default::default()
    })
    .exec_without_returning(conn)
    .await?;
    create_user(
        conn,
        admin_user_id,
        tenant_id,
        admin_email,
        Role::Admin,
        issuer,
        subject,
    )
    .await
}

/// OIDC identity と所属を同じ INSERT で作成する。
pub async fn create_user(
    executor: &impl ConnectionTrait,
    user_id: &str,
    tenant_id: &str,
    email: &str,
    role: Role,
    issuer: &str,
    subject: &str,
) -> Result<(), DbErr> {
    users::Entity::insert(users::ActiveModel {
        id: Set(user_id.into()),
        tenant_id: Set(tenant_id.into()),
        email: Set(email.into()),
        role: Set(role.as_str().into()),
        oidc_issuer: Set(Some(issuer.into())),
        oidc_subject: Set(Some(subject.into())),
        ..Default::default()
    })
    .exec_without_returning(executor)
    .await?;
    Ok(())
}

/// テナント内ユーザを id で解決し role を返す（トークン発行の対象ユーザ確認用）。
///
/// テナント外/不在は `None`（IDOR: 存在秘匿のため呼び出し側で 404 にする）。
pub async fn find_user_role(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    user_id: &str,
) -> Result<Option<String>, DbErr> {
    users::Entity::find()
        .select_only()
        .column(users::Column::Role)
        .filter(users::Column::TenantId.eq(tenant_id))
        .filter(users::Column::Id.eq(user_id))
        .filter(users::Column::DeletedAt.is_null())
        .into_tuple::<String>()
        .one(executor)
        .await
}

/// slug からテナントを解決する（login のテナント解決, §3.3）。active のみ。
///
/// OIDC 開始時の認証前参照（GUC 無し）。固定の SECURITY DEFINER 関数を呼び、
/// 未一致の 0 行は `None` として返す。
pub async fn find_tenant_id_by_slug(
    executor: &impl ConnectionTrait,
    slug: &str,
) -> Result<Option<String>, DbErr> {
    function_rows(executor, "auth_lookup_tenant_id_by_slug", vec![slug.into()])
        .await?
        .first()
        .map(|r| r.try_get("", "id"))
        .transpose()
}

pub enum TokenAuthMethod {
    Api,
    Oidc,
}

impl TokenAuthMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Oidc => "oidc",
        }
    }
}

/// Lock the caller and optional existing target in a stable order, then the
/// calling token. Identity creation, linking and token issuance hold these locks
/// through commit, in the same order as user-wide revocation.
pub async fn lock_identity_actor(
    executor: &impl ConnectionTrait,
    principal: &Principal,
    target_user: Option<&str>,
) -> Result<Option<TokenRow>, DbErr> {
    let mut user_ids: Vec<&str> = target_user.into_iter().collect();
    if let Some(id) = principal.user_id.as_deref() {
        user_ids.push(id);
    }
    let locked_users = users::Entity::find()
        .filter(users::Column::TenantId.eq(&principal.tenant_id))
        .filter(users::Column::Id.is_in(user_ids))
        .filter(users::Column::DeletedAt.is_null())
        .order_by_asc(users::Column::Id)
        .lock_exclusive()
        .all(executor)
        .await?;
    let Some(token) = api_tokens::Entity::find_by_id(&principal.token_id)
        .filter(api_tokens::Column::TenantId.eq(&principal.tenant_id))
        .lock_exclusive()
        .one(executor)
        .await?
    else {
        return Ok(None);
    };
    if token.user_id != principal.user_id {
        return Ok(None);
    }
    let user = match token.user_id.as_deref() {
        Some(id) => match locked_users.iter().find(|u| u.id == id) {
            Some(user) if user.auth_version == token.user_auth_version => Some(user),
            _ => return Ok(None),
        },
        None => None,
    };
    Ok(Some(TokenRow {
        token_id: token.id,
        tenant_id: token.tenant_id,
        user_id: token.user_id,
        scopes: token.scopes,
        expires_at: token.expires_at,
        revoked_at: token.revoked_at,
        user_role: user.map(|u| u.role.clone()),
        auth_method: token.auth_method,
        user_oidc_issuer: user.and_then(|u| u.oidc_issuer.clone()),
    }))
}

/// API トークン行を作成する。`scopes` は文字列配列で渡す（CHECK 制約に合致させる）。
#[allow(clippy::too_many_arguments)]
pub async fn create_token(
    executor: &impl ConnectionTrait,
    token_id: &str,
    tenant_id: &str,
    user_id: Option<&str>,
    token_hash: &str,
    scopes: &[String],
    name: Option<&str>,
    expires_at: DateTime<Utc>,
    auth_method: TokenAuthMethod,
) -> Result<(), DbErr> {
    let user_auth_version = if let Some(user_id) = user_id {
        lock_user(executor, tenant_id, user_id)
            .await?
            .ok_or_else(|| DbErr::Custom("token owner is not active".into()))?
            .auth_version
    } else {
        0
    };
    api_tokens::Entity::insert(api_tokens::ActiveModel {
        id: Set(token_id.into()),
        tenant_id: Set(tenant_id.into()),
        user_id: Set(user_id.map(str::to_owned)),
        token_hash: Set(token_hash.into()),
        scopes: Set(scopes.to_vec()),
        name: Set(name.map(str::to_owned)),
        expires_at: Set(expires_at),
        user_auth_version: Set(user_auth_version),
        auth_method: Set(auth_method.as_str().into()),
        ..Default::default()
    })
    .exec_without_returning(executor)
    .await?;
    Ok(())
}

/// トークンを失効させる（DELETE /tokens/{id}）。
///
/// IDOR 対策: 必ず呼び出し主体の `tenant_id` でスコープする。更新 0 行なら
/// テナント外/不在（呼び出し側は 404 にする＝存在秘匿）。
pub async fn revoke_token(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    token_id: &str,
) -> Result<u64, DbErr> {
    Ok(api_tokens::Entity::update_many()
        .col_expr(api_tokens::Column::RevokedAt, now())
        .filter(api_tokens::Column::TenantId.eq(tenant_id))
        .filter(api_tokens::Column::Id.eq(token_id))
        .filter(api_tokens::Column::RevokedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected)
}

/// 指定 token_id がテナント内に存在するか（revoke の 404/204 判定補助）。
pub async fn token_exists(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    token_id: &str,
) -> Result<bool, DbErr> {
    hibana_database::queries::exists(
        executor,
        api_tokens::Entity::find()
            .filter(api_tokens::Column::TenantId.eq(tenant_id))
            .filter(api_tokens::Column::Id.eq(token_id)),
    )
    .await
}

/// Call inside the caller's tenant transaction. Serializes issuance with revocation.
pub async fn lock_user(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    user_id: &str,
) -> Result<Option<users::Model>, DbErr> {
    users::Entity::find_by_id(user_id)
        .filter(users::Column::TenantId.eq(tenant_id))
        .filter(users::Column::DeletedAt.is_null())
        .lock_exclusive()
        .one(executor)
        .await
}

/// Session display is read-only; it must not wait for credential-changing locks.
pub async fn user_email(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    user_id: &str,
) -> Result<Option<String>, DbErr> {
    users::Entity::find_by_id(user_id)
        .select_only()
        .column(users::Column::Email)
        .filter(users::Column::TenantId.eq(tenant_id))
        .filter(users::Column::DeletedAt.is_null())
        .into_tuple()
        .one(executor)
        .await
}

pub async fn find_oidc_user(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    issuer: &str,
    subject: &str,
) -> Result<Option<users::Model>, DbErr> {
    users::Entity::find()
        .filter(users::Column::TenantId.eq(tenant_id))
        .filter(users::Column::OidcIssuer.eq(issuer))
        .filter(users::Column::OidcSubject.eq(subject))
        .filter(users::Column::DeletedAt.is_null())
        .one(executor)
        .await
}

pub async fn bind_oidc_user(
    executor: &impl ConnectionTrait,
    tenant: &str,
    user: &str,
    issuer: &str,
    subject: &str,
) -> Result<(), DbErr> {
    users::Entity::update_many()
        .col_expr(users::Column::OidcIssuer, Expr::value(issuer))
        .col_expr(users::Column::OidcSubject, Expr::value(subject))
        .col_expr(
            users::Column::AuthVersion,
            Expr::col(users::Column::AuthVersion).add(1),
        )
        .filter(users::Column::TenantId.eq(tenant))
        .filter(users::Column::Id.eq(user))
        .exec(executor)
        .await?;
    Ok(())
}

/// Update display metadata in the login transaction after locking the user.
pub async fn update_user_email(
    executor: &impl ConnectionTrait,
    tenant: &str,
    user: &str,
    email: &str,
) -> Result<(), DbErr> {
    users::Entity::update_many()
        .col_expr(users::Column::Email, Expr::value(email))
        .filter(users::Column::TenantId.eq(tenant))
        .filter(users::Column::Id.eq(user))
        .filter(users::Column::DeletedAt.is_null())
        .exec(executor)
        .await?;
    Ok(())
}

pub async fn revoke_user_tokens(
    executor: &impl ConnectionTrait,
    tenant: &str,
    user: &str,
    disable: bool,
) -> Result<(), DbErr> {
    let mut update = users::Entity::update_many()
        .col_expr(
            users::Column::AuthVersion,
            Expr::col(users::Column::AuthVersion).add(1),
        )
        .filter(users::Column::TenantId.eq(tenant))
        .filter(users::Column::Id.eq(user));
    if disable {
        update = update.col_expr(users::Column::DeletedAt, now());
    }
    update.exec(executor).await?;
    // Keep explicit revocation timestamps for operators and audit tooling as well.
    api_tokens::Entity::update_many()
        .col_expr(api_tokens::Column::RevokedAt, now())
        .filter(api_tokens::Column::TenantId.eq(tenant))
        .filter(api_tokens::Column::UserId.eq(user))
        .filter(api_tokens::Column::RevokedAt.is_null())
        .exec(executor)
        .await?;
    Ok(())
}

pub async fn list_users(
    executor: &impl ConnectionTrait,
    tenant: &str,
) -> Result<Vec<(String, String, String, Option<String>)>, DbErr> {
    users::Entity::find()
        .select_only()
        .columns([
            users::Column::Id,
            users::Column::Email,
            users::Column::Role,
            users::Column::OidcSubject,
        ])
        .filter(users::Column::TenantId.eq(tenant))
        .filter(users::Column::DeletedAt.is_null())
        .order_by_asc(users::Column::Email)
        .into_tuple()
        .all(executor)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use hibana_shared::Scope;

    fn row(user: Option<&str>, role: Option<&str>) -> TokenRow {
        TokenRow {
            token_id: "token".into(),
            tenant_id: "tenant".into(),
            user_id: user.map(str::to_owned),
            user_role: role.map(str::to_owned),
            scopes: ["admin", "unknown", "read", "deploy", "invoke"]
                .map(String::from)
                .to_vec(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            revoked_at: None,
            auth_method: "api".into(),
            user_oidc_issuer: None,
        }
    }

    #[test]
    fn deleted_or_missing_user_never_becomes_service_admin() {
        assert!(row(Some("deleted"), None).into_principal().is_none());
        assert!(row(Some("user"), Some("unknown"))
            .into_principal()
            .is_none());
        assert!(row(None, Some("admin")).into_principal().is_none());
        assert_eq!(row(None, None).into_principal().unwrap().role, Role::Admin);
    }

    #[test]
    fn scopes_preserve_order_and_respect_roles_without_granting_unknown_scopes() {
        for (user, role, expected_role, expected_scopes) in [
            (
                Some("user"),
                Some("member"),
                Role::Member,
                vec![Scope::Read, Scope::Deploy],
            ),
            (
                Some("user"),
                Some("admin"),
                Role::Admin,
                vec![Scope::Admin, Scope::Read, Scope::Deploy],
            ),
            (
                None,
                None,
                Role::Admin,
                vec![Scope::Admin, Scope::Read, Scope::Deploy],
            ),
        ] {
            let principal = row(user, role).into_principal().unwrap();
            assert_eq!(principal.role, expected_role);
            assert_eq!(principal.scopes, expected_scopes);
        }
    }
}
