//! Audit persistence.
use serde_json::Value;

// ---------------------------------------------------------------------------
// 追記専用 audit ログ (§3.2 / §3.7, M3c)
// ---------------------------------------------------------------------------

pub async fn insert_audit_log(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str,
    actor: Option<&str>,
    action: &str,
    target: Option<&str>,
    detail: Option<&Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (tenant_id, actor, action, target, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(executor)
    .await?;
    Ok(())
}
