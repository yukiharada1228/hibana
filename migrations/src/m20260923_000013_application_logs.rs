//! Tenant-scoped output shares execution provenance and its existing FORCE RLS.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table("executions")
                    .add_column(ColumnDef::new("application_logs").json_binary().null())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_executions_retained_logs")
                    .table("executions")
                    .col("tenant_id")
                    .col("finished_at")
                    .col("id")
                    .and_where(Expr::col("application_logs").is_not_null())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_executions_app_created")
                    .table("executions")
                    .col("tenant_id")
                    .col("component_id")
                    .col(("created_at", IndexOrder::Desc))
                    .col(("id", IndexOrder::Desc))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Application logs migration cannot be rolled back: retained logs would be lost.".into(),
        ))
    }
}
