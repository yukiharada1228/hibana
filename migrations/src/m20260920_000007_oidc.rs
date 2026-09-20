//! OIDC identities and immediate user-wide token revocation.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(include_str!("m20260920_000007_oidc.sql"))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        // Generations disappear on rollback. Persist their revocations before
        // restoring the legacy lookup; lock out concurrent identity/token writes.
        connection
            .execute_unprepared(
                r#"
            LOCK TABLE users, api_tokens IN ACCESS EXCLUSIVE MODE;
            UPDATE api_tokens t SET revoked_at = now()
            WHERE t.revoked_at IS NULL AND t.user_id IS NOT NULL
              AND NOT EXISTS (
                SELECT 1 FROM users u
                WHERE u.id = t.user_id AND u.tenant_id = t.tenant_id
                  AND u.deleted_at IS NULL AND u.auth_version = t.user_auth_version
              );
        "#,
            )
            .await?;
        // Restore the legacy body without dropping its stable result contract,
        // before removing the columns used by the newer body.
        let original = include_str!("m20260915_000001_security.sql");
        let start = original
            .find("CREATE OR REPLACE FUNCTION public.auth_lookup_token_by_hash")
            .unwrap();
        let end = original[start..]
            .find("GRANT EXECUTE ON FUNCTION auth_lookup_token_by_hash(text) TO faas_app;")
            .unwrap()
            + start
            + "GRANT EXECUTE ON FUNCTION auth_lookup_token_by_hash(text) TO faas_app;".len();
        connection.execute_unprepared(&original[start..end]).await?;
        connection.execute_unprepared(r#"
            DROP FUNCTION public.auth_lookup_token_by_hash_v2(text);
            DROP TRIGGER classify_legacy_token_auth ON public.api_tokens;
            DROP FUNCTION public.classify_legacy_token_auth();
            DROP TRIGGER snapshot_legacy_token_generation ON public.api_tokens;
            DROP FUNCTION public.snapshot_legacy_token_generation();
            DROP FUNCTION public.legacy_token_auth_method(text, text, text);
            DROP INDEX public.audit_logs_token_issued;
            ALTER TABLE api_tokens DROP COLUMN auth_method, DROP COLUMN user_auth_version;
            ALTER TABLE users DROP COLUMN oidc_issuer, DROP COLUMN oidc_subject, DROP COLUMN auth_version;
        "#).await?;
        Ok(())
    }
}
