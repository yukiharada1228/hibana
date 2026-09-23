use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // NULL preserves legacy per-version grants. Only an administrator can
        // explicitly establish a policy shared by existing and future versions.
        manager
            .alter_table(
                Table::alter()
                    .table("components")
                    .add_column(ColumnDef::new("egress_policy").json_binary())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table("components")
                    .drop_column("egress_policy")
                    .to_owned(),
            )
            .await
    }
}
