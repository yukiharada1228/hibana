-- Stop old Control Planes before applying this migration. Existing rows remain
-- available for auditing, but all credentials and pending OIDC grants expire.
LOCK TABLE users, api_tokens IN ACCESS EXCLUSIVE MODE;
UPDATE users SET auth_version = auth_version + 1;
UPDATE api_tokens SET revoked_at = now() WHERE revoked_at IS NULL;

DROP TRIGGER classify_legacy_token_auth ON public.api_tokens;
DROP FUNCTION public.classify_legacy_token_auth();
DROP TRIGGER snapshot_legacy_token_generation ON public.api_tokens;
DROP FUNCTION public.snapshot_legacy_token_generation();
DROP FUNCTION public.legacy_token_auth_method(text, text, text);
DROP INDEX public.audit_logs_token_issued;

-- Issuers must explicitly supply both the authentication method and the
-- owner's generation. Historical methods are retained only on revoked rows.
ALTER TABLE api_tokens
    ALTER COLUMN auth_method DROP DEFAULT,
    ALTER COLUMN user_auth_version DROP DEFAULT,
    ADD CONSTRAINT api_tokens_current_auth CHECK (
        auth_method IN ('api', 'oidc') OR revoked_at IS NOT NULL
    );

DROP FUNCTION public.auth_lookup_user_by_email(text, text);
ALTER TABLE users DROP COLUMN password_hash;

-- Keep a single lookup contract, including identity generation and issuer.
DROP FUNCTION public.auth_lookup_token_by_hash(text);
ALTER FUNCTION public.auth_lookup_token_by_hash_v2(text)
    RENAME TO auth_lookup_token_by_hash;
