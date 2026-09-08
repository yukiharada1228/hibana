-- Deletion releases the public name while retaining historical IDs and records.
ALTER TABLE components DROP CONSTRAINT components_tenant_id_name_key;
CREATE UNIQUE INDEX components_live_tenant_name
    ON components (tenant_id, name) WHERE deleted_at IS NULL;
