-- Wrangler version metadata follows the same tenant boundary as native components.
CREATE UNIQUE INDEX uq_components_tenant_id ON components (tenant_id, id);
CREATE UNIQUE INDEX uq_component_versions_tenant_id ON component_versions (tenant_id, id);
CREATE TABLE wrangler_workers (
    tenant_id TEXT NOT NULL,
    component_id TEXT NOT NULL,
    PRIMARY KEY (tenant_id, component_id),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id) ON DELETE CASCADE
);
ALTER TABLE wrangler_workers ENABLE ROW LEVEL SECURITY;
ALTER TABLE wrangler_workers FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON wrangler_workers
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON wrangler_workers TO faas_app;
CREATE TABLE wrangler_versions (
    tenant_id TEXT NOT NULL,
    id TEXT NOT NULL,
    version_id TEXT NOT NULL,
    metadata JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, version_id),
    FOREIGN KEY (tenant_id, version_id) REFERENCES component_versions (tenant_id, id) ON DELETE CASCADE
);
ALTER TABLE wrangler_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE wrangler_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON wrangler_versions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON wrangler_versions TO faas_app;
