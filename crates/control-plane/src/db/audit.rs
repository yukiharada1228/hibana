//! Audit persistence.
use hibana_database::prelude::*;

use serde_json::Value;

// ---------------------------------------------------------------------------
// 追記専用 audit ログ (§3.2 / §3.7, M3c)
// ---------------------------------------------------------------------------

pub async fn insert_audit_log(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    actor: Option<&str>,
    action: &str,
    target: Option<&str>,
    detail: Option<&Value>,
) -> Result<(), DbErr> {
    audit_logs::Entity::insert(audit_logs::ActiveModel {
        tenant_id: Set(tenant_id.into()),
        actor: Set(actor.map(str::to_owned)),
        action: Set(action.into()),
        target: Set(target.map(str::to_owned)),
        detail: Set(detail.cloned()),
        ..Default::default()
    })
    .exec(executor)
    .await?;
    Ok(())
}
