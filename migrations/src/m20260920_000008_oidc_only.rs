//! Remove the password-era runtime contract. This is a one-way cutover.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(include_str!("m20260920_000008_oidc_only.sql"))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "OIDC-only cutover cannot be rolled back: password hashes were removed. Restore a pre-cutover backup instead.".into(),
        ))
    }
}
