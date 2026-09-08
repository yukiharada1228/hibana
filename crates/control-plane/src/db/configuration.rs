//! Immutable environment of the active version. Values are plaintext, never Secrets.
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct FunctionConfigRow {
    pub key: String,
    pub value: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn list_function_configs(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<FunctionConfigRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT v.key, v.value, v.updated_at FROM version_configs v \
         JOIN components c ON c.tenant_id = v.tenant_id AND c.id = v.component_id \
           AND c.active_version_id = v.version_id \
         WHERE v.tenant_id = $1 AND v.component_id = $2 AND c.deleted_at IS NULL ORDER BY v.key",
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
