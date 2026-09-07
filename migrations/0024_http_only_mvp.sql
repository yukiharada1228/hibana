-- Keep historical rows and old migration checksums intact. Only new HTTP requests
-- may be redeemed by the synchronous Worker endpoint.
ALTER TABLE executions ADD COLUMN http_request boolean NOT NULL DEFAULT false;
