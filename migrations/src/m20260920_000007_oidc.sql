ALTER TABLE users
    ADD COLUMN oidc_issuer text,
    ADD COLUMN oidc_subject text,
    ADD COLUMN auth_version bigint NOT NULL DEFAULT 0,
    ADD CONSTRAINT users_oidc_pair CHECK ((oidc_issuer IS NULL) = (oidc_subject IS NULL));
CREATE UNIQUE INDEX users_oidc_identity ON users(tenant_id, oidc_issuer, oidc_subject);
ALTER TABLE api_tokens
    ADD COLUMN auth_method text NOT NULL DEFAULT 'legacy' CHECK (auth_method IN ('api', 'password', 'oidc', 'legacy')),
    ADD COLUMN user_auth_version bigint NOT NULL DEFAULT 0;
-- Names are user-controlled. Only an unambiguous issuance audit proves origin.
-- Missing/conflicting provenance is treated as password-origin, so it cannot
-- survive disabling password authentication. Keep lookups bounded by token ID.
CREATE INDEX audit_logs_token_issued ON audit_logs(tenant_id, target)
    WHERE action = 'token_issued';
CREATE FUNCTION public.legacy_token_auth_method(p_tenant text, p_token text, p_user text)
RETURNS text
LANGUAGE sql STABLE
SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
    SELECT CASE WHEN count(*) = 1 THEN min(
        CASE
            WHEN detail->>'via' = 'oidc' AND actor = p_user THEN 'oidc'
            WHEN NOT (detail ? 'via') AND detail ? 'user_id'
                 AND (detail->>'user_id') IS NOT DISTINCT FROM p_user THEN 'api'
            ELSE 'password'
        END
    ) ELSE 'password' END
    FROM audit_logs
    WHERE tenant_id = p_tenant AND target = p_token AND action = 'token_issued'
$function$;
REVOKE ALL ON FUNCTION public.legacy_token_auth_method(text, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.legacy_token_auth_method(text, text, text) TO faas_app;
UPDATE api_tokens SET auth_method = public.legacy_token_auth_method(tenant_id, id, user_id);

-- Old writers also omit the owner's generation. Snapshot it BEFORE insertion,
-- under the same user lock as modern issuance, linking and revocation. Never
-- refresh a pre-existing token's generation at classification/commit time.
CREATE FUNCTION public.snapshot_legacy_token_generation()
RETURNS trigger
LANGUAGE plpgsql
SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
BEGIN
    IF NEW.user_id IS NOT NULL THEN
        SELECT auth_version INTO NEW.user_auth_version
        FROM users
        WHERE tenant_id = NEW.tenant_id AND id = NEW.user_id AND deleted_at IS NULL
        FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'token owner is not active' USING ERRCODE = '23503';
        END IF;
    END IF;
    RETURN NEW;
END;
$function$;
REVOKE ALL ON FUNCTION public.snapshot_legacy_token_generation() FROM PUBLIC;
CREATE TRIGGER snapshot_legacy_token_generation
BEFORE INSERT ON public.api_tokens
FOR EACH ROW WHEN (NEW.auth_method = 'legacy')
EXECUTE FUNCTION public.snapshot_legacy_token_generation();

-- Old Control Planes omit auth_method while a rolling upgrade is in progress.
-- Their audit row is inserted AFTER the token, in the same transaction. Defer
-- classification until commit. New writers set the method explicitly and never
-- enter this compatibility path. The transient 'legacy' value fails closed in
-- the new authenticator even if constraints are forced early by a writer.
CREATE FUNCTION public.classify_legacy_token_auth()
RETURNS trigger
LANGUAGE plpgsql
SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
BEGIN
    UPDATE api_tokens
    SET auth_method = public.legacy_token_auth_method(tenant_id, id, user_id)
    WHERE tenant_id = NEW.tenant_id AND id = NEW.id AND auth_method = 'legacy';
    RETURN NULL;
END;
$function$;
REVOKE ALL ON FUNCTION public.classify_legacy_token_auth() FROM PUBLIC;
CREATE CONSTRAINT TRIGGER classify_legacy_token_auth
AFTER INSERT ON public.api_tokens
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (NEW.auth_method = 'legacy')
EXECUTE FUNCTION public.classify_legacy_token_auth();

-- Preserve the legacy function's OID and seven-column result contract. Old CP
-- connections cache SELECT * prepared statements throughout a rolling upgrade.
CREATE OR REPLACE FUNCTION public.auth_lookup_token_by_hash(p_hash text)
RETURNS TABLE(id text, tenant_id text, user_id text, scopes text[], expires_at timestamptz,
              revoked_at timestamptz, user_role text)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
    SELECT t.id, t.tenant_id, t.user_id, t.scopes, t.expires_at, t.revoked_at, u.role
    FROM api_tokens t
    LEFT JOIN users u ON u.id = t.user_id AND u.tenant_id = t.tenant_id
                      AND u.deleted_at IS NULL AND u.auth_version = t.user_auth_version
    WHERE t.token_hash = p_hash AND (t.user_id IS NULL OR u.id IS NOT NULL)
$function$;
REVOKE ALL ON FUNCTION public.auth_lookup_token_by_hash(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.auth_lookup_token_by_hash(text) TO faas_app;

CREATE FUNCTION public.auth_lookup_token_by_hash_v2(p_hash text)
RETURNS TABLE(id text, tenant_id text, user_id text, scopes text[], expires_at timestamptz,
              revoked_at timestamptz, user_role text, auth_method text, user_oidc_issuer text)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path TO 'pg_catalog', 'public', 'pg_temp'
AS $function$
    SELECT t.id, t.tenant_id, t.user_id, t.scopes, t.expires_at, t.revoked_at, u.role, t.auth_method, u.oidc_issuer
    FROM api_tokens t
    LEFT JOIN users u ON u.id = t.user_id AND u.tenant_id = t.tenant_id
                      AND u.deleted_at IS NULL AND u.auth_version = t.user_auth_version
    WHERE t.token_hash = p_hash AND (t.user_id IS NULL OR u.id IS NOT NULL)
$function$;
REVOKE ALL ON FUNCTION public.auth_lookup_token_by_hash_v2(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.auth_lookup_token_by_hash_v2(text) TO faas_app;
