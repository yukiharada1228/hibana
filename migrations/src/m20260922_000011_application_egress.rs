//! Application egress is the only authority; version grants are removed.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Drain and stop old Control Planes and Workers before applying this.
        // Never promote a historical version grant into an application grant.
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE components SET egress_policy = '[]'::jsonb WHERE egress_policy IS NULL;
                 ALTER TABLE components
                     ALTER COLUMN egress_policy SET DEFAULT '[]'::jsonb,
                     ALTER COLUMN egress_policy SET NOT NULL,
                     ADD CONSTRAINT components_egress_policy_array
                         CHECK (jsonb_typeof(egress_policy) = 'array');
                 UPDATE component_versions
                 SET capabilities = capabilities - 'net_allow_outbound'
                 WHERE jsonb_typeof(capabilities) = 'object'
                     AND capabilities ? 'net_allow_outbound';",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Application egress cutover cannot be rolled back: version-specific grants were removed."
                .into(),
        ))
    }
}
