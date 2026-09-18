//! Keep periodic cleanup proportional to retained inputs, not all execution history.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .name("idx_executions_retained_input")
                    .table("executions")
                    .col("tenant_id")
                    .col("id")
                    .cond_where(
                        Cond::any()
                            .add(Expr::col("input").is_not_null())
                            .add(Expr::col("input_ref").is_not_null()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_executions_retained_input")
                    .table("executions")
                    .to_owned(),
            )
            .await
    }
}
