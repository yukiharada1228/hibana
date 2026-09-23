//! Retain encryption keys needed by accepted requests as well as current Secrets.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// PostgreSQL SECURITY DEFINER DDL stays in migrations. Runtime callers receive
// only key IDs/counts through the existing, restricted function interface.
const REQUIRED_GENERATIONS: &str = r#"
    WITH required AS (
        SELECT s.tenant_id, s.id AS secret_id, s.current_version AS version
        FROM function_secrets s
        WHERE s.deleted_at IS NULL
        UNION
        SELECT s.tenant_id, s.id, generation.version
        FROM executions e
        JOIN version_secret_bindings b
          ON b.tenant_id = e.tenant_id AND b.component_id = e.component_id
         AND b.version_id = e.version_id
        JOIN function_secrets s
          ON s.tenant_id = b.tenant_id AND s.component_id = b.component_id
         AND s.id = b.secret_id AND s.name = b.name
        JOIN LATERAL (
            SELECT v.version FROM function_secret_versions v
            WHERE v.tenant_id = s.tenant_id AND v.secret_id = s.id
              AND v.created_at <= e.created_at
            ORDER BY v.version DESC LIMIT 1
        ) generation ON true
        WHERE e.status IN ('pending', 'running') AND s.deleted_at IS NULL
    )
    SELECT v.kek_kid, count(*)
    FROM required r JOIN function_secret_versions v
      ON v.tenant_id = r.tenant_id AND v.secret_id = r.secret_id AND v.version = r.version
    GROUP BY v.kek_kid
"#;

const CURRENT_GENERATIONS: &str = r#"
    SELECT v.kek_kid, count(*)
    FROM function_secrets s JOIN function_secret_versions v
      ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
    WHERE s.deleted_at IS NULL
    GROUP BY v.kek_kid
"#;

async fn replace_counter(manager: &SchemaManager<'_>, query: &str) -> Result<(), DbErr> {
    manager
        .get_connection()
        .execute_unprepared(&format!(
            r#"
        CREATE OR REPLACE FUNCTION public.secrets_kek_kid_counts_all()
        RETURNS TABLE(kek_kid text, n bigint)
        LANGUAGE sql STABLE SECURITY DEFINER
        SET search_path TO 'pg_catalog', 'public', 'pg_temp'
        AS $function$ {query} $function$;
        REVOKE ALL ON FUNCTION public.secrets_kek_kid_counts_all() FROM PUBLIC;
        GRANT EXECUTE ON FUNCTION public.secrets_kek_kid_counts_all() TO faas_app;
    "#
        ))
        .await?;
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        replace_counter(manager, REQUIRED_GENERATIONS).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        replace_counter(manager, CURRENT_GENERATIONS).await
    }
}
