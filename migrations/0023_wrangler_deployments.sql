-- A deployment is an event, distinct from the immutable code version.
CREATE TABLE wrangler_deployments (
    tenant_id TEXT NOT NULL,
    id TEXT NOT NULL,
    component_id TEXT NOT NULL,
    version_id TEXT NOT NULL,
    annotations JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, version_id) REFERENCES wrangler_versions (tenant_id, id)
);
CREATE INDEX wrangler_deployment_history ON wrangler_deployments (tenant_id, component_id, created_at DESC);
ALTER TABLE wrangler_deployments ENABLE ROW LEVEL SECURITY;
ALTER TABLE wrangler_deployments FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON wrangler_deployments
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON wrangler_deployments TO faas_app;

-- Preserve the currently deployed pre-migration version. Historical events
-- cannot be reconstructed, so mark this baseline explicitly rather than inventing them.
INSERT INTO wrangler_deployments (tenant_id,id,component_id,version_id,annotations)
SELECT c.tenant_id,w.id,c.id,w.id,'{"hibana/baseline":"migration-0023"}'::jsonb
FROM components c JOIN wrangler_versions w
  ON w.tenant_id=c.tenant_id AND w.version_id=c.active_version_id
WHERE c.deleted_at IS NULL;
