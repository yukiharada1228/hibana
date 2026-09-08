-- Code and environment are published together. Stop/drain both old CP and workers
-- before migrating: old binaries still read/write the legacy function_configs table.
ALTER TABLE component_versions ADD CONSTRAINT component_versions_tenant_component_id_key
    UNIQUE (tenant_id, component_id, id);
ALTER TABLE function_secrets ADD CONSTRAINT function_secrets_tenant_component_id_name_key
    UNIQUE (tenant_id, component_id, id, name);
ALTER TABLE function_secrets ADD COLUMN deploy_allowed BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE version_configs (
    tenant_id TEXT NOT NULL,
    component_id TEXT NOT NULL,
    version_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, version_id, key),
    FOREIGN KEY (tenant_id, component_id, version_id)
        REFERENCES component_versions (tenant_id, component_id, id)
);
CREATE TABLE version_secret_bindings (
    tenant_id TEXT NOT NULL,
    component_id TEXT NOT NULL,
    version_id TEXT NOT NULL,
    secret_id TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (tenant_id, version_id, name),
    FOREIGN KEY (tenant_id, component_id, version_id)
        REFERENCES component_versions (tenant_id, component_id, id),
    FOREIGN KEY (tenant_id, component_id, secret_id, name)
        REFERENCES function_secrets (tenant_id, component_id, id, name)
);
ALTER TABLE version_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE version_configs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON version_configs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
ALTER TABLE version_secret_bindings ENABLE ROW LEVEL SECURITY;
ALTER TABLE version_secret_bindings FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON version_secret_bindings
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- No historical vars were recorded. The upgrade baseline is the current config
-- for every existing version. Preserve old explicit env approvals, never grant
-- these Secrets to future uploads automatically. Pin the identity, not a reusable name.
DO $$
DECLARE tenant RECORD;
BEGIN
    FOR tenant IN SELECT id FROM tenants LOOP
        PERFORM set_config('app.tenant_id', tenant.id, true);
        INSERT INTO version_configs (tenant_id, component_id, version_id, key, value, updated_at)
        SELECT v.tenant_id, v.component_id, v.id, c.key, c.value, c.updated_at
        FROM component_versions v JOIN function_configs c
          ON c.tenant_id = v.tenant_id AND c.component_id = v.component_id
        WHERE v.tenant_id = tenant.id;
        INSERT INTO version_secret_bindings (tenant_id, component_id, version_id, secret_id, name)
        SELECT v.tenant_id, v.component_id, v.id, s.id, s.name
        FROM component_versions v JOIN function_secrets s
          ON s.tenant_id = v.tenant_id AND s.component_id = v.component_id
        WHERE v.tenant_id = tenant.id AND s.deleted_at IS NULL
          AND (CASE WHEN jsonb_typeof(v.capabilities->'env') = 'array'
                    THEN v.capabilities->'env' ELSE '[]'::jsonb END) ? s.name;
    END LOOP;
END $$;
REVOKE ALL ON version_configs, version_secret_bindings FROM PUBLIC;
GRANT SELECT, INSERT ON version_configs, version_secret_bindings TO faas_app;
-- Keep the legacy baseline for operators, but reject old writers during upgrades.
REVOKE INSERT, UPDATE, DELETE ON function_configs FROM faas_app;
