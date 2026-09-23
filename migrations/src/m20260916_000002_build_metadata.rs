use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // NULL means unrecorded; an explicitly empty declaration means no extensions.
        manager
            .alter_table(
                Table::alter()
                    .table("component_versions")
                    .add_column(ColumnDef::new("build_metadata").json_binary())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table("component_versions")
                    .drop_column("build_metadata")
                    .to_owned(),
            )
            .await
    }
}
