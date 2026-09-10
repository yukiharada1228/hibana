-- Retry failed deletes fairly without extending artifact retention leases.
ALTER TABLE artifact_reservations ADD COLUMN cleanup_retry_at timestamptz NOT NULL
    DEFAULT '-infinity'::timestamptz;
CREATE INDEX artifact_reservations_cleanup
    ON artifact_reservations (tenant_id, cleanup_retry_at, expires_at, id);
