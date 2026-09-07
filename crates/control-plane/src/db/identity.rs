//! Identity persistence.
use crate::auth::Principal;
use chrono::DateTime;
use chrono::Utc;
use faas_shared::Role;
use faas_shared::Scope;
use sqlx::Row;

// ---------------------------------------------------------------------------
// 認証・認可: users / api_tokens (§3.3 / §6.0)
// ---------------------------------------------------------------------------

/// `api_tokens` の照合結果行（認証ホットパス）。
///
/// 失効/期限の判定は `into_principal`（純関数化のため `is_token_valid` に委譲）で行う。
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub token_id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// 紐づくユーザのロール。サービストークン（user_id NULL）は `None`。
    pub user_role: Option<String>,
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

/// 文字列スコープ集合を `Scope` ベクタへパースする。未知の文字列は破棄する。
pub fn parse_scopes(raw: &[String]) -> Vec<Scope> {
    raw.iter()
        .filter_map(|s| match s.as_str() {
            "read" => Some(Scope::Read),
            "invoke" => Some(Scope::Invoke),
            "deploy" => Some(Scope::Deploy),
            "admin" => Some(Scope::Admin),
            _ => None,
        })
        .collect()
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
        let role = match self.user_role.as_deref() {
            None => Role::Admin, // サービストークン（ユーザ無し）。
            other => parse_role(other)?,
        };
        Some(Principal {
            tenant_id: self.tenant_id,
            user_id: self.user_id,
            token_id: self.token_id,
            scopes: parse_scopes(&self.scopes),
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
/// RLS 対象外）を呼ぶことで、GUC 無しの認証前参照を成立させる（migrations/0004_rls.sql）。
pub async fn find_token_by_hash(
    executor: impl sqlx::PgExecutor<'_>,
    token_hash: &str,
) -> Result<Option<TokenRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, tenant_id, user_id, scopes, expires_at, revoked_at, user_role \
         FROM auth_lookup_token_by_hash($1)",
    )
    .bind(token_hash)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(TokenRow {
            token_id: r.try_get("id")?,
            tenant_id: r.try_get("tenant_id")?,
            user_id: r.try_get("user_id")?,
            scopes: r.try_get("scopes")?,
            expires_at: r.try_get("expires_at")?,
            revoked_at: r.try_get("revoked_at")?,
            user_role: r.try_get("user_role")?,
        })
    })
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
/// M3b: マルチステートメント（tenants + users INSERT を 1 単位で実行）のため
/// `impl PgExecutor` ではなく `&mut PgConnection` を受け、tx は呼び出し側が所有する
/// （内部 begin/commit は撤去）。呼び出し側は **同一 tx 上で先に**
/// `set_tenant_guc(&mut *tx, tenant_id)` を呼ぶこと: users は FORCE RLS 下で
/// WITH CHECK が GUC と一致する必要がある（tenants には RLS は無い）。
#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_tenant(
    conn: &mut sqlx::PgConnection,
    tenant_id: &str,
    slug: &str,
    name: &str,
    admin_user_id: &str,
    admin_email: &str,
    admin_password_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO tenants (id, slug, name, status) VALUES ($1, $2, $3, 'active')")
        .bind(tenant_id)
        .bind(slug)
        .bind(name)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'admin')",
    )
    .bind(admin_user_id)
    .bind(tenant_id)
    .bind(admin_email)
    .bind(admin_password_hash)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// ユーザを作成する（POST /tenants/{id}/users）。`tenant_id` でスコープする。
pub async fn create_user(
    executor: impl sqlx::PgExecutor<'_>,
    user_id: &str,
    tenant_id: &str,
    email: &str,
    password_hash: &str,
    role: Role,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(email)
    .bind(password_hash)
    .bind(role.as_str())
    .execute(executor)
    .await?;
    Ok(())
}

/// login / トークン発行で参照するユーザ行。
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub password_hash: String,
    pub role: String,
}

/// (tenant_id, email) でユーザを解決する（soft-delete 済みは除外）。
///
/// M3b: login の認証前参照（Principal 確立前。slug 解決の後だが GUC はまだ無い）。
/// users は FORCE RLS のため GUC 無しでは読めないので、SECURITY DEFINER 関数
/// `auth_lookup_user_by_email` を呼ぶ（migrations/0004_rls.sql）。
pub async fn find_user_by_email(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    email: &str,
) -> Result<Option<UserRow>, sqlx::Error> {
    let row = sqlx::query("SELECT id, password_hash, role FROM auth_lookup_user_by_email($1, $2)")
        .bind(tenant_id)
        .bind(email)
        .fetch_optional(executor)
        .await?;

    row.map(|r| {
        Ok(UserRow {
            id: r.try_get("id")?,
            password_hash: r.try_get("password_hash")?,
            role: r.try_get("role")?,
        })
    })
    .transpose()
}

/// テナント内ユーザを id で解決し role を返す（トークン発行の対象ユーザ確認用）。
///
/// テナント外/不在は `None`（IDOR: 存在秘匿のため呼び出し側で 404 にする）。
pub async fn find_user_role(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT role FROM users \
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| r.try_get::<String, _>("role")).transpose()
}

/// slug からテナントを解決する（login のテナント解決, §3.3）。active のみ。
///
/// M3b: login の最初の認証前参照（GUC 無し）。tenants 自体には RLS は無いが、規約として
/// 3 つの認証前参照を SECURITY DEFINER 関数経由に統一する。`auth_lookup_tenant_id_by_slug`
/// は `RETURNS TABLE(id text)` のため、未一致は 0 行 → `fetch_optional` で `None`（スカラ
/// NULL 行の誤判定を避ける, migrations/0004_rls.sql）。
pub async fn find_tenant_id_by_slug(
    executor: impl sqlx::PgExecutor<'_>,
    slug: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT id FROM auth_lookup_tenant_id_by_slug($1)")
        .bind(slug)
        .fetch_optional(executor)
        .await?;

    row.map(|r| r.try_get::<String, _>("id")).transpose()
}

/// API トークン行を作成する。`scopes` は文字列配列で渡す（CHECK 制約に合致させる）。
#[allow(clippy::too_many_arguments)]
pub async fn create_token(
    executor: impl sqlx::PgExecutor<'_>,
    token_id: &str,
    tenant_id: &str,
    user_id: Option<&str>,
    token_hash: &str,
    scopes: &[String],
    name: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_tokens \
         (id, tenant_id, user_id, token_hash, scopes, name, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(token_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .bind(scopes)
    .bind(name)
    .bind(expires_at)
    .execute(executor)
    .await?;
    Ok(())
}

/// トークンを失効させる（DELETE /tokens/{id}）。
///
/// IDOR 対策: 必ず呼び出し主体の `tenant_id` でスコープする。更新 0 行なら
/// テナント外/不在（呼び出し側は 404 にする＝存在秘匿）。
pub async fn revoke_token(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    token_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() \
         WHERE tenant_id = $1 AND id = $2 AND revoked_at IS NULL",
    )
    .bind(tenant_id)
    .bind(token_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// 指定 token_id がテナント内に存在するか（revoke の 404/204 判定補助）。
pub async fn token_exists(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    token_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM api_tokens WHERE tenant_id = $1 AND id = $2) AS present",
    )
    .bind(tenant_id)
    .bind(token_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}
