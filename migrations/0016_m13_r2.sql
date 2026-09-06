-- M13: R2 バインディング（Cloudflare Workers R2 互換, オブジェクトストレージ）のストア。
--
-- **MVP はオブジェクトを Postgres(bytea) に保持する**。理由: worker は keyless by design
-- （§3.3。S3 資格情報を持たず、CP 発行の短命 presigned URL しか使わない）。R2 を MinIO で正しく
-- やるには「worker→CP presign→S3 転送」の往復が要る（keyless を保つため）。それは後続。ここでは
-- KV と同じ `send_request` 横取り + Postgres で、worker を keyless のまま R2 API を通す。
-- 大きすぎるオブジェクトは worker 側で上限（R2_MAX_OBJECT_BYTES）拒否する。将来 MinIO+CP-presign へ
-- 差し替え可能（API 契約は不変）。
--
-- テナント境界は RLS（app.tenant_id GUC）。bucket は tenant 内パーティション（tenant 内 component 間の
-- セキュリティ境界ではない。KV の namespace と同じ扱い）。
CREATE TABLE IF NOT EXISTS r2_objects (
    tenant_id    TEXT        NOT NULL REFERENCES tenants(id),
    bucket       TEXT        NOT NULL,
    key          TEXT        NOT NULL,
    value        BYTEA       NOT NULL,
    content_type TEXT,
    custom_meta  JSONB       NOT NULL DEFAULT '{}'::jsonb,
    size         BIGINT      NOT NULL,
    etag         TEXT        NOT NULL,
    uploaded_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, bucket, key)
);

CREATE INDEX IF NOT EXISTS idx_r2_objects_prefix
    ON r2_objects (tenant_id, bucket, key);

ALTER TABLE r2_objects ENABLE ROW LEVEL SECURITY;
ALTER TABLE r2_objects FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON r2_objects
    USING      (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

GRANT SELECT, INSERT, UPDATE, DELETE ON r2_objects TO faas_app;
