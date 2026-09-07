-- Acceptance and delivery intent must commit together. Store object keys, not expiring credentials.
CREATE UNIQUE INDEX uq_executions_tenant_id ON executions (tenant_id, id);

CREATE TABLE execution_outbox (
    tenant_id TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    payload JSONB NOT NULL,
    published_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    error_class TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, execution_id),
    FOREIGN KEY (tenant_id, execution_id) REFERENCES executions (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX execution_outbox_due ON execution_outbox (next_attempt_at) WHERE published_at IS NULL;
ALTER TABLE execution_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON execution_outbox
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON execution_outbox TO faas_app;

-- Successful finalization and the intent to fan out chains share the same transaction.
CREATE TABLE chain_outbox (
    tenant_id TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, execution_id),
    FOREIGN KEY (tenant_id, execution_id) REFERENCES executions (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX chain_outbox_due ON chain_outbox (next_attempt_at);
ALTER TABLE chain_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE chain_outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON chain_outbox
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
GRANT SELECT, INSERT, UPDATE, DELETE ON chain_outbox TO faas_app;

-- The cross-tenant scanner exposes identifiers only; payload reads and writes still require RLS.
CREATE FUNCTION delivery_outbox_due()
    RETURNS TABLE (tenant_id TEXT, execution_id TEXT, kind TEXT)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT pending.tenant_id, pending.execution_id, pending.kind
    FROM (
        SELECT o.tenant_id, o.execution_id, 'job'::text AS kind, o.next_attempt_at
        FROM execution_outbox o WHERE o.published_at IS NULL
        UNION ALL
        SELECT o.tenant_id, o.execution_id, 'chain'::text, o.next_attempt_at FROM chain_outbox o
    ) pending JOIN tenants t ON t.id = pending.tenant_id AND t.status = 'active'
    WHERE pending.next_attempt_at <= now()
    ORDER BY pending.next_attempt_at, pending.tenant_id, pending.execution_id
    LIMIT 64;
$$;
REVOKE EXECUTE ON FUNCTION delivery_outbox_due() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION delivery_outbox_due() TO faas_app;

CREATE FUNCTION delivery_outbox_stats()
    RETURNS TABLE (pending_jobs BIGINT, oldest_seconds BIGINT)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT count(*), COALESCE(ceil(extract(epoch FROM now()-min(created_at)))::bigint,0)
    FROM execution_outbox WHERE published_at IS NULL;
$$;
REVOKE EXECUTE ON FUNCTION delivery_outbox_stats() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION delivery_outbox_stats() TO faas_app;
