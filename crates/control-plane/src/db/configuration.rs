//! Immutable environment of the active version. Values are plaintext, never Secrets.
use hibana_database::prelude::*;

#[derive(Debug, Clone, FromQueryResult)]
pub struct FunctionConfigRow {
    pub key: String,
    pub value: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn list_function_configs(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<FunctionConfigRow>, DbErr> {
    version_configs::Entity::find()
        .select_only()
        .columns([
            version_configs::Column::Key,
            version_configs::Column::Value,
            version_configs::Column::UpdatedAt,
        ])
        .filter(version_configs::Column::TenantId.eq(tenant_id))
        .filter(version_configs::Column::ComponentId.eq(component_id))
        .filter(
            version_configs::Column::VersionId.in_subquery(
                components::Entity::find()
                    .select_only()
                    .column(components::Column::ActiveVersionId)
                    .filter(components::Column::TenantId.eq(tenant_id))
                    .filter(components::Column::Id.eq(component_id))
                    .filter(components::Column::DeletedAt.is_null())
                    .into_query(),
            ),
        )
        .order_by_asc(version_configs::Column::Key)
        .into_model::<FunctionConfigRow>()
        .all(executor)
        .await
}
