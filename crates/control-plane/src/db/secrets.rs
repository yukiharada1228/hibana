//! Secrets persistence.
use sqlx::Row;

// ---------------------------------------------------------------------------
// M7c: Secrets Manager（function_secrets / function_secret_versions, migration 0011）
//
// **平文も暗号文もこの層より上へ素で流さない**。値は `secrets.rs` の封筒 [`crate::secrets::Envelope`]
// としてのみ受け渡す。版台帳は追記専用（faas_app に UPDATE/DELETE が無い）なので、値の更新
// （rotate）も KEK 再ラップ（rekey）も「新しい version 行の INSERT」で表現する。
// すべて set_tenant_guc 済み tx で呼ぶこと。
// ---------------------------------------------------------------------------

/// `function_secrets` のメタデータ 1 行（**値は含まない**）。
#[derive(Debug, Clone)]
pub struct SecretMetaRow {
    pub id: String,
    pub name: String,
    pub current_version: i32,
    /// 作成時刻。M7c-3 の execution 基準の世代解決（§4.7）で参照する。
    #[allow(dead_code)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// secret メタ行を作成する（版行は別途 `insert_secret_version` で INSERT する）。
///
/// 生存行の同名重複は部分 UNIQUE index 違反（23505）。呼び出し側が捕捉して
/// 「既存 → rotate」へ倒す。
pub async fn insert_secret_meta(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    component_id: &str,
    name: &str,
    current_version: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_secrets \
         (id, tenant_id, component_id, name, current_version) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(secret_id)
    .bind(tenant_id)
    .bind(component_id)
    .bind(name)
    .bind(current_version)
    .execute(executor)
    .await?;
    Ok(())
}

/// 生存している secret を名前で引く（**メタのみ**）。
pub async fn find_live_secret_by_name(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<Option<SecretMetaRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, current_version, created_at, updated_at FROM function_secrets \
          WHERE tenant_id = $1 AND component_id = $2 AND name = $3 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(name)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(SecretMetaRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            current_version: r.try_get("current_version")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
        })
    })
    .transpose()
}

/// component の生存 secret を全件引く（**メタのみ**。値も value_len も kek_kid も返さない）。
pub async fn list_secrets_meta(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<SecretMetaRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, current_version, created_at, updated_at FROM function_secrets \
          WHERE tenant_id = $1 AND component_id = $2 AND deleted_at IS NULL ORDER BY name",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(SecretMetaRow {
                id: r.try_get("id")?,
                name: r.try_get("name")?,
                current_version: r.try_get("current_version")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}

/// 版台帳へ 1 行 INSERT する（追記専用）。`reason` は `'create' | 'rotate' | 'rekey'`。
pub async fn insert_secret_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
    env: &crate::secrets::Envelope,
    reason: &str,
    created_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_secret_versions \
         (tenant_id, secret_id, version, kek_kid, wrapped_dek, dek_nonce, nonce, ciphertext, \
          value_len, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .bind(&env.kek_kid)
    .bind(&env.wrapped_dek)
    .bind(&env.dek_nonce)
    .bind(&env.nonce)
    .bind(&env.ciphertext)
    .bind(env.value_len)
    .bind(reason)
    .bind(created_by)
    .execute(executor)
    .await?;
    Ok(())
}

/// `current_version` を前進させる（rotate / rekey 後の切替）。
pub async fn bump_secret_current_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE function_secrets SET current_version = $3, updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// secret を soft delete する。版台帳は残る（追記専用なので消せない ＝ 監査上も残す）。
///
/// 生存行の部分 UNIQUE index から外れるため、**同名で作り直せる**（インシデント対応の基本操作）。
pub async fn soft_delete_secret(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE function_secrets SET deleted_at = now(), updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn component_has_live_secrets(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS ( \
             SELECT 1 FROM function_secrets \
              WHERE tenant_id = $1 AND component_id = $2 AND deleted_at IS NULL \
         ) AS present",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_one(executor)
    .await?;
    row.try_get("present")
}

/// 現行 kid でない current 世代を持つ secret の id を引く（M7c-4: rekey 対象）。
///
/// `secrets_stale_kek(p_tenant, p_active_kid)` は SECURITY DEFINER（faas_app は FORCE RLS 下で
/// 巡回できない）。**テナント引数を取る版**なので GUC 前でも呼べるが、返すのは
/// `(tenant_id, secret_id)` だけで暗号文も名前も返さない（0004 の認証前参照 3 関数と同じ作法）。
pub async fn secrets_stale_kek(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    active_kid: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT secret_id FROM secrets_stale_kek($1, $2)")
        .bind(tenant_id)
        .bind(active_kid)
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| r.try_get::<String, _>("secret_id"))
        .collect()
}

/// kid 別の current 世代件数（M7c-4: Prometheus gauge の内部更新専用）。
///
/// **HTTP 応答に載せてはならない** (MUST NOT)。全テナント横断の集計であり、テナント管理者へ
/// 返すと他テナントの secret 総数が漏れる。
pub async fn secrets_kek_kid_counts_all(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows = sqlx::query("SELECT kek_kid, n FROM secrets_kek_kid_counts_all()")
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| Ok((r.try_get("kek_kid")?, r.try_get("n")?)))
        .collect()
}

/// secret メタ行を id で引く（rekey が current 世代を解決するのに使う）。
pub async fn find_secret_meta_by_id(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
) -> Result<Option<SecretMetaRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, current_version, created_at, updated_at FROM function_secrets \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(SecretMetaRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            current_version: r.try_get("current_version")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
        })
    })
    .transpose()
}

/// 指定 secret の指定版の封筒を引く。
pub async fn find_secret_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<Option<crate::secrets::Envelope>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT kek_kid, wrapped_dek, dek_nonce, nonce, ciphertext, value_len \
           FROM function_secret_versions \
          WHERE tenant_id = $1 AND secret_id = $2 AND version = $3",
    )
    .bind(tenant_id)
    .bind(secret_id)
    .bind(version)
    .fetch_optional(executor)
    .await?;

    row.map(|r| {
        Ok(crate::secrets::Envelope {
            kek_kid: r.try_get("kek_kid")?,
            wrapped_dek: r.try_get("wrapped_dek")?,
            dek_nonce: r.try_get("dek_nonce")?,
            nonce: r.try_get("nonce")?,
            ciphertext: r.try_get("ciphertext")?,
            value_len: r.try_get("value_len")?,
        })
    })
    .transpose()
}
