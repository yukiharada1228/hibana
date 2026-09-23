//! Request payloads are temporary dispatch data, not execution history.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        use sea_orm::{ConnectionTrait, TransactionTrait};
        let tx = manager.get_connection().begin().await?;
        let tenants = tx
            .query_all(&Query::select().column("id").from("tenants").to_owned())
            .await?;
        // Include suspended tenants and respect FORCE RLS without granting the
        // application role migration privileges. Pending/running jobs stay intact.
        for tenant in tenants {
            let id: String = tenant.try_get("", "id")?;
            tx.query_one(
                &Query::select()
                    .expr(Func::cust("set_config").args([
                        Expr::val("app.tenant_id"),
                        Expr::val(id.clone()),
                        Expr::val(true),
                    ]))
                    .to_owned(),
            )
            .await?;
            tx.execute(
                &Query::update()
                    .table("executions")
                    .value("input", SimpleExpr::Keyword(Keyword::Null))
                    .value("input_ref", SimpleExpr::Keyword(Keyword::Null))
                    .and_where(Expr::col("tenant_id").eq(id))
                    .and_where(Expr::col("http_request").eq(true))
                    .and_where(Expr::col("status").is_not_in(["pending", "running"]))
                    .cond_where(
                        Cond::any()
                            .add(Expr::col("input").is_not_null())
                            .add(Expr::col("input_ref").is_not_null()),
                    )
                    .to_owned(),
            )
            .await?;
        }
        tx.commit().await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Intentionally irreversible: discarded credentials must not be restored.
        Ok(())
    }
}
