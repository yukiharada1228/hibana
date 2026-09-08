//! Signing keys persistence.
use sqlx::Row;

// ---------------------------------------------------------------------------
// M9a: Component 署名鍵（component_signing_keys, migration 0012）
// ---------------------------------------------------------------------------

/// テナントが登録した署名鍵の 1 行。
#[derive(Debug, Clone)]
pub struct SigningKey {
    pub key_id: String,
    /// Ed25519 公開鍵（base64url, パディング無し）。
    pub public_key: String,
    /// 'active' | 'retired'。
    pub status: String,
}

/// テナントの署名鍵を全件返す（active + retired）。検証は全鍵を試すので status で絞らない。
pub async fn list_signing_keys(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<Vec<SigningKey>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT key_id, public_key, status FROM component_signing_keys \
          WHERE tenant_id = $1 ORDER BY created_at ASC, key_id ASC FOR SHARE",
    )
    .bind(tenant_id)
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SigningKey {
                key_id: r.try_get("key_id")?,
                public_key: r.try_get("public_key")?,
                status: r.try_get("status")?,
            })
        })
        .collect()
}

/// 署名鍵を登録する（同一 key_id は公開鍵 / status を上書き = 冪等な再登録）。
pub async fn upsert_signing_key(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    key_id: &str,
    public_key: &str,
    created_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO component_signing_keys (tenant_id, key_id, public_key, status, created_by) \
         VALUES ($1, $2, $3, 'active', $4) \
         ON CONFLICT (tenant_id, key_id) \
         DO UPDATE SET public_key = EXCLUDED.public_key, status = 'active'",
    )
    .bind(tenant_id)
    .bind(key_id)
    .bind(public_key)
    .bind(created_by)
    .execute(executor)
    .await?;
    Ok(())
}

/// 鍵を retire する（検証は通すが新規署名の推奨から外す）。0 行 = 不在 → 呼び出し側が 404。
pub async fn retire_signing_key(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    key_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE component_signing_keys SET status = 'retired' \
          WHERE tenant_id = $1 AND key_id = $2",
    )
    .bind(tenant_id)
    .bind(key_id)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// テナントが「署名必須」ポリシーかどうか（`tenants.require_signed_components`）。
pub async fn require_signed_components(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT require_signed_components FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(executor)
        .await?;
    Ok(row
        .map(|r| r.try_get::<bool, _>("require_signed_components"))
        .transpose()?
        // テナント行が引けない = 未知テナント。安全側（署名必須）に倒す。
        .unwrap_or(true))
}

/// 署名必須ポリシーを設定する。0 行 = テナント不在 → 呼び出し側が 404。
pub async fn set_require_signed_components(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    require: bool,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("UPDATE tenants SET require_signed_components = $2 WHERE id = $1")
        .bind(tenant_id)
        .bind(require)
        .execute(executor)
        .await?;
    Ok(r.rows_affected() > 0)
}
