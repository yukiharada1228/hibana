//! ORM migration entry point. A new baseline requires an empty database.
use sea_orm::{ConnectionTrait, DatabaseConnection};

pub(crate) async fn run_migrate_only() -> anyhow::Result<()> {
    use anyhow::Context as _;
    let url = std::env::var("MIGRATION_DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .context("MIGRATION_DATABASE_URL or DATABASE_URL must be set for --migrate-only")?;
    let pool = hibana_database::postgres::connect(&url, 2, 0).await?;
    run_migrations(&pool).await?;
    pool.close().await?;
    Ok(())
}

pub(crate) async fn run_migrations(pool: &DatabaseConnection) -> anyhow::Result<()> {
    // Do not upgrade an unrelated/legacy database by interpreting its tables as
    // the new baseline. Existing production data is switched separately.
    hibana_migration::migrate(pool).await?;
    Ok(())
}

pub(crate) async fn assert_non_privileged_runtime_role(
    pool: &impl ConnectionTrait,
) -> anyhow::Result<()> {
    hibana_database::postgres::assert_runtime_role(pool).await?;
    Ok(())
}
