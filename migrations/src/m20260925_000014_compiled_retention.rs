use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
            CREATE OR REPLACE FUNCTION public.hibana_protected_artifact_hashes()
             RETURNS TABLE(sha256 text)
             LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path TO 'pg_catalog', 'public', 'pg_temp'
            AS $function$
                SELECT v.wasm_sha256 FROM components c JOIN component_versions v
                  ON v.id IN (c.active_version_id, c.previous_active_version_id)
                  AND v.tenant_id=c.tenant_id AND v.component_id=c.id
                  WHERE c.deleted_at IS NULL AND v.deleted_at IS NULL
                UNION
                SELECT v.wasm_sha256 FROM executions e JOIN component_versions v
                  ON v.id=e.version_id AND v.tenant_id=e.tenant_id AND v.component_id=e.component_id
                  WHERE e.status IN ('pending','running')
                UNION
                SELECT wasm_sha256 FROM artifact_reservations WHERE expires_at > now()
            $function$;
            REVOKE ALL ON FUNCTION public.hibana_protected_artifact_hashes() FROM PUBLIC;
            GRANT EXECUTE ON FUNCTION public.hibana_protected_artifact_hashes() TO faas_app;
        "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Compiled retention cannot be rolled back: rollback targets must remain protected."
                .into(),
        ))
    }
}
