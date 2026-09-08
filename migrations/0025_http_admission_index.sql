-- Keep authoritative admission proportional to outstanding requests, not history.
CREATE INDEX idx_executions_http_inflight
    ON executions (tenant_id, created_at)
    WHERE http_request AND status IN ('pending', 'running');
