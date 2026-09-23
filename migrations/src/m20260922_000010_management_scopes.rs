//! Remove the obsolete invocation permission; public app HTTP uses no API scope.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE api_tokens DROP CONSTRAINT api_tokens_scopes_check;
             UPDATE api_tokens SET scopes = array_remove(scopes, 'invoke')
             WHERE 'invoke' = ANY(scopes);
             ALTER TABLE api_tokens ADD CONSTRAINT api_tokens_scopes_check
             CHECK (scopes <@ ARRAY['read', 'deploy', 'admin']::text[]);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Management scope cleanup cannot be rolled back: obsolete token scopes were removed."
                .into(),
        ))
    }
}
