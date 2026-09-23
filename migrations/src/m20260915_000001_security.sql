-- RLS, runtime privileges and narrowly scoped SECURITY DEFINER functions.

DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='faas_app') THEN
        CREATE ROLE faas_app LOGIN PASSWORD 'faas_app' NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOINHERIT;
    END IF;
END $$;
GRANT USAGE ON SCHEMA public TO faas_app;
GRANT SELECT ON TABLE public.seaql_migrations TO faas_app;

ALTER TABLE users ENABLE ROW LEVEL SECURITY;
ALTER TABLE users FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON users
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON users FROM PUBLIC;

ALTER TABLE api_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE api_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON api_tokens
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON api_tokens FROM PUBLIC;

ALTER TABLE audit_logs ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_logs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON audit_logs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON audit_logs FROM PUBLIC;

ALTER TABLE components ENABLE ROW LEVEL SECURITY;
ALTER TABLE components FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON components
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON components FROM PUBLIC;

ALTER TABLE component_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE component_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON component_versions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON component_versions FROM PUBLIC;

ALTER TABLE executions ENABLE ROW LEVEL SECURITY;
ALTER TABLE executions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON executions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON executions FROM PUBLIC;

ALTER TABLE usage_rollups ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_rollups FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON usage_rollups
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON usage_rollups FROM PUBLIC;

ALTER TABLE function_secrets ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secrets FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON function_secrets
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON function_secrets FROM PUBLIC;

ALTER TABLE function_secret_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secret_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON function_secret_versions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON function_secret_versions FROM PUBLIC;

ALTER TABLE component_signing_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE component_signing_keys FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON component_signing_keys
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON component_signing_keys FROM PUBLIC;

ALTER TABLE version_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE version_configs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON version_configs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON version_configs FROM PUBLIC;

ALTER TABLE version_secret_bindings ENABLE ROW LEVEL SECURITY;
ALTER TABLE version_secret_bindings FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON version_secret_bindings
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON version_secret_bindings FROM PUBLIC;

ALTER TABLE artifact_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifact_reservations FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON artifact_reservations
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
REVOKE ALL ON artifact_reservations FROM PUBLIC;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.api_tokens TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.artifact_reservations TO faas_app;

GRANT SELECT,INSERT ON TABLE public.audit_logs TO faas_app;

GRANT SELECT,USAGE ON SEQUENCE public.audit_logs_id_seq TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.component_signing_keys TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.component_versions TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.components TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.executions TO faas_app;

GRANT SELECT,INSERT ON TABLE public.function_secret_versions TO faas_app;

GRANT SELECT,INSERT,UPDATE ON TABLE public.function_secrets TO faas_app;

GRANT SELECT ON TABLE public.platform_maintenance TO faas_app;

GRANT SELECT,INSERT ON TABLE public.tenants TO faas_app;

GRANT UPDATE(status) ON TABLE public.tenants TO faas_app;

GRANT UPDATE(quotas) ON TABLE public.tenants TO faas_app;

GRANT UPDATE(require_signed_components) ON TABLE public.tenants TO faas_app;

GRANT SELECT,INSERT,UPDATE ON TABLE public.usage_rollups TO faas_app;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE public.users TO faas_app;

GRANT SELECT,INSERT ON TABLE public.version_configs TO faas_app;

GRANT SELECT,INSERT ON TABLE public.version_secret_bindings TO faas_app;

INSERT INTO platform_maintenance(singleton,owner) VALUES (true,NULL);


CREATE OR REPLACE FUNCTION public.auth_lookup_tenant_id_by_slug(p_slug text)
 RETURNS TABLE(id text)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
        SELECT id FROM tenants WHERE slug = p_slug AND status = 'active'
    $function$;

REVOKE ALL ON FUNCTION auth_lookup_tenant_id_by_slug(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION auth_lookup_tenant_id_by_slug(text) TO faas_app;
CREATE OR REPLACE FUNCTION public.auth_lookup_token_by_hash(p_hash text)
 RETURNS TABLE(id text, tenant_id text, user_id text, scopes text[], expires_at timestamp with time zone, revoked_at timestamp with time zone, user_role text)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
        SELECT t.id, t.tenant_id, t.user_id, t.scopes, t.expires_at, t.revoked_at, u.role
        FROM api_tokens t
        LEFT JOIN users u ON u.id = t.user_id AND u.tenant_id = t.tenant_id AND u.deleted_at IS NULL
        WHERE t.token_hash = p_hash
    $function$;

REVOKE ALL ON FUNCTION auth_lookup_token_by_hash(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION auth_lookup_token_by_hash(text) TO faas_app;
CREATE OR REPLACE FUNCTION public.auth_lookup_user_by_email(p_tenant text, p_email text)
 RETURNS TABLE(id text, password_hash text, role text)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
        SELECT id, password_hash, role FROM users
        WHERE tenant_id = p_tenant AND email = p_email AND deleted_at IS NULL
    $function$;

REVOKE ALL ON FUNCTION auth_lookup_user_by_email(text,text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION auth_lookup_user_by_email(text,text) TO faas_app;
CREATE OR REPLACE FUNCTION public.hibana_protected_artifact_hashes()
 RETURNS TABLE(sha256 text)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
    SELECT v.wasm_sha256 FROM components c JOIN component_versions v
      ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id
      WHERE c.deleted_at IS NULL AND v.deleted_at IS NULL
    UNION
    SELECT v.wasm_sha256 FROM executions e JOIN component_versions v
      ON v.id=e.version_id AND v.tenant_id=e.tenant_id AND v.component_id=e.component_id
      WHERE e.status IN ('pending','running')
    UNION
    SELECT wasm_sha256 FROM artifact_reservations WHERE expires_at > now()
$function$;

REVOKE ALL ON FUNCTION hibana_protected_artifact_hashes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION hibana_protected_artifact_hashes() TO faas_app;
CREATE OR REPLACE FUNCTION public.hibana_set_maintenance(p_owner text, p_closed boolean)
 RETURNS boolean
 LANGUAGE sql
 SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
    WITH changed AS (
        UPDATE platform_maintenance SET owner=CASE WHEN p_closed THEN p_owner ELSE NULL END
        WHERE singleton AND p_owner <> '' AND
          (owner=p_owner OR (p_closed AND owner IS NULL))
        RETURNING singleton
    ) SELECT EXISTS(SELECT 1 FROM changed)
$function$;

REVOKE ALL ON FUNCTION hibana_set_maintenance(text,boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION hibana_set_maintenance(text,boolean) TO faas_app;
CREATE OR REPLACE FUNCTION public.secrets_kek_kid_counts_all()
 RETURNS TABLE(kek_kid text, n bigint)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
        SELECT v.kek_kid, count(*)
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.deleted_at IS NULL
        GROUP BY v.kek_kid
    $function$;

REVOKE ALL ON FUNCTION secrets_kek_kid_counts_all() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION secrets_kek_kid_counts_all() TO faas_app;
CREATE OR REPLACE FUNCTION public.secrets_stale_kek(p_tenant text, p_active_kid text)
 RETURNS TABLE(tenant_id text, secret_id text)
 LANGUAGE sql
 STABLE SECURITY DEFINER
 SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
        SELECT s.tenant_id, s.id
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.tenant_id = p_tenant AND s.deleted_at IS NULL AND v.kek_kid <> p_active_kid
    $function$;

REVOKE ALL ON FUNCTION secrets_stale_kek(text,text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION secrets_stale_kek(text,text) TO faas_app;
