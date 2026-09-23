//! Immutable environment of the active version. Values are plaintext, never Secrets.
use hibana_database::prelude::*;
use sea_orm::DerivePartialModel;

#[derive(Debug, Clone, DerivePartialModel)]
#[sea_orm(entity = "version_configs::Entity")]
pub struct FunctionConfigRow {
    pub key: String,
    pub value: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn active_configs(tenant_id: &str, component_id: &str) -> sea_orm::Select<version_configs::Entity> {
    version_configs::Entity::find()
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
}

pub async fn list_function_configs(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<FunctionConfigRow>, DbErr> {
    active_configs(tenant_id, component_id)
        .order_by_asc(version_configs::Column::Key)
        .into_partial_model::<FunctionConfigRow>()
        .all(executor)
        .await
}

/// Secret name collision checks must never fetch plaintext configuration values.
pub async fn function_config_key_exists(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<bool, DbErr> {
    hibana_database::queries::exists(
        executor,
        active_configs(tenant_id, component_id).filter(version_configs::Column::Key.eq(name)),
    )
    .await
}
