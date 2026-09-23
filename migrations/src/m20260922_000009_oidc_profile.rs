//! OIDC email is mutable display metadata, not a unique identity key.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(Index::drop().name("users_tenant_id_email_key").to_owned())
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "OIDC profile migration cannot be rolled back: different identities may share an email. Restore a pre-migration backup instead.".into(),
        ))
    }
}
