//! Versioned migrations for a new Hibana platform database.
use sea_orm_migration::prelude::*;
pub use sea_orm_migration::MigratorTrait;
mod m20260915_000001_indexes;
mod m20260915_000001_platform;
mod m20260916_000002_build_metadata;
mod m20260916_000003_component_egress;
mod m20260917_000004_execution_input_retention;
mod m20260917_000005_execution_input_cleanup;
mod m20260918_000006_secret_key_retention;

pub struct Migrator;

/// Serialize migration attempts, including first installation of the history
/// table. PostgreSQL rolls back both schema and history on any failure.
pub async fn migrate(db: &sea_orm::DatabaseConnection) -> Result<(), DbErr> {
    use sea_orm::{ConnectionTrait, TransactionTrait};
    let tx = db.begin().await?;
    tx.query_one(
        &Query::select()
            .expr(Func::cust("pg_advisory_xact_lock").arg(Expr::val(0x484942414e41_i64)))
            .to_owned(),
    )
    .await?;
    let manager = SchemaManager::new(&tx);
    if manager.has_table("_sqlx_migrations").await? {
        return Err(empty_database_error());
    }
    if !manager.has_table("seaql_migrations").await? {
        assert_empty_schema(&manager).await?;
    }
    Migrator::up(&tx, None).await?;
    tx.commit().await
}

fn empty_database_error() -> DbErr {
    DbErr::Custom("This baseline requires an empty database; legacy Hibana databases must be switched separately.".into())
}

async fn assert_empty_schema(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let table = manager
        .get_connection()
        .query_one(
            &Query::select()
                .column("table_name")
                .from(("information_schema", "tables"))
                .and_where(Expr::col("table_schema").eq("public"))
                .and_where(Expr::col("table_name").ne("seaql_migrations"))
                .limit(1)
                .to_owned(),
        )
        .await?;
    if table.is_some() {
        return Err(empty_database_error());
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260915_000001_platform::Migration),
            Box::new(m20260916_000002_build_metadata::Migration),
            Box::new(m20260916_000003_component_egress::Migration),
            Box::new(m20260917_000004_execution_input_retention::Migration),
            Box::new(m20260917_000005_execution_input_cleanup::Migration),
            Box::new(m20260918_000006_secret_key_retention::Migration),
        ]
    }
}
