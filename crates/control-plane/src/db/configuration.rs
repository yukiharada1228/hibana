//! Configuration persistence.
use sqlx::Row;

// ---------------------------------------------------------------------------
// M7b: per-function の平文環境変数（function_configs, migration 0010）
//
// **平文である**ことが型と権限の両方で分かるようにする（secret は function_secrets 側で
// 暗号化して持ち、読み出し API を持たない）。すべて set_tenant_guc 済み tx で呼ぶこと。
// ---------------------------------------------------------------------------

/// `function_configs` の 1 行（平文なので値も返す）。
#[derive(Debug, Clone)]
pub struct FunctionConfigRow {
    pub key: String,
    pub value: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// per-function config を 1 キー upsert する。
pub async fn upsert_function_config(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    key: &str,
    value: &str,
    updated_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO function_configs (tenant_id, component_id, key, value, updated_by) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, component_id, key) \
         DO UPDATE SET value = EXCLUDED.value, updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(key)
    .bind(value)
    .bind(updated_by)
    .execute(executor)
    .await?;
    Ok(())
}

/// component の config を全件引く（キー名昇順 ＝ 応答と注入の決定性）。
pub async fn list_function_configs(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<FunctionConfigRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT key, value, updated_at FROM function_configs \
          WHERE tenant_id = $1 AND component_id = $2 ORDER BY key",
    )
    .bind(tenant_id)
    .bind(component_id)
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(FunctionConfigRow {
                key: r.try_get("key")?,
                value: r.try_get("value")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}

/// config を 1 キー削除する。戻り値は削除が起きたか（0 行 → 404）。
pub async fn delete_function_config(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "DELETE FROM function_configs WHERE tenant_id = $1 AND component_id = $2 AND key = $3",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(key)
    .execute(executor)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// component の config を全置換する（`PUT /config` のトランザクション内で使う）。
///
/// 「全置換」を DELETE + INSERT で表現する。同一 tx 内なので中間状態は観測されない。
pub async fn replace_function_configs(
    tx: &mut sqlx::PgConnection,
    tenant_id: &str,
    component_id: &str,
    entries: &[(String, String)],
    updated_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM function_configs WHERE tenant_id = $1 AND component_id = $2")
        .bind(tenant_id)
        .bind(component_id)
        .execute(&mut *tx)
        .await?;
    for (key, value) in entries {
        upsert_function_config(&mut *tx, tenant_id, component_id, key, value, updated_by).await?;
    }
    Ok(())
}
