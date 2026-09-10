-- Journal uploads before contacting S3, and pin artifacts until publication finishes.
CREATE TABLE artifact_reservations (
    id text PRIMARY KEY,
    tenant_id text NOT NULL REFERENCES tenants(id),
    version_id text NOT NULL,
    storage_uri text NOT NULL,
    wasm_sha256 text NOT NULL,
    expires_at timestamptz NOT NULL DEFAULT now() + interval '5 minutes'
);
CREATE INDEX artifact_reservations_expiry ON artifact_reservations (tenant_id, expires_at);
ALTER TABLE artifact_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifact_reservations FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON artifact_reservations
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON artifact_reservations TO faas_app;

-- Workers need only digests, never cross-tenant metadata or storage credentials.
CREATE FUNCTION hibana_protected_artifact_hashes() RETURNS TABLE (sha256 text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    SELECT v.wasm_sha256 FROM components c JOIN component_versions v
      ON v.id=c.active_version_id AND v.tenant_id=c.tenant_id AND v.component_id=c.id
      WHERE c.deleted_at IS NULL AND v.deleted_at IS NULL
    UNION
    SELECT v.wasm_sha256 FROM executions e JOIN component_versions v
      ON v.id=e.version_id AND v.tenant_id=e.tenant_id
      WHERE e.status IN ('pending','running')
    UNION
    SELECT wasm_sha256 FROM artifact_reservations WHERE expires_at > now()
$$;
REVOKE ALL ON FUNCTION hibana_protected_artifact_hashes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION hibana_protected_artifact_hashes() TO faas_app;

-- The authenticated operator API uses a compare-and-set; it cannot take over a backup.
CREATE FUNCTION hibana_set_maintenance(p_owner text, p_closed boolean) RETURNS boolean
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    WITH changed AS (
        UPDATE platform_maintenance SET owner=CASE WHEN p_closed THEN p_owner ELSE NULL END
        WHERE singleton AND p_owner <> '' AND
          (owner=p_owner OR (p_closed AND owner IS NULL))
        RETURNING singleton
    ) SELECT EXISTS(SELECT 1 FROM changed)
$$;
REVOKE ALL ON FUNCTION hibana_set_maintenance(text,boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION hibana_set_maintenance(text,boolean) TO faas_app;
