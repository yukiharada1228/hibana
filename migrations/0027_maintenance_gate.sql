-- Platform-wide admission gate. Only the migration/operator role may change it.
-- This is operational state, not tenant data. A restored backup stays closed.
CREATE TABLE platform_maintenance (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    owner text
);
INSERT INTO platform_maintenance (singleton) VALUES (true);
REVOKE ALL ON platform_maintenance FROM PUBLIC, faas_app;
GRANT SELECT ON platform_maintenance TO faas_app;
