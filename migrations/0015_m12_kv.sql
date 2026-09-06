-- M12: KV バインディング（Cloudflare Workers KV 互換）のストア。
--
-- 0001-0014 への ADD のみ（新規テーブル＝backfill 不要）。sqlx migrate ランナーが順次適用する
-- （BASELINE_VERSIONS には入れない）。テーブル所有ロール（MIGRATION_DATABASE_URL）で実行する。
--
-- 設計:
--  - **テナント境界は RLS**（app.tenant_id GUC）。worker は KV アクセス時に必ず「ジョブの
--    tenant_id」を GUC に設定してからクエリするため、guest がキー/namespace を自由に指定しても
--    **自テナントの外へは絶対に出られない**（cross-tenant は構造的に不可能）。
--  - `namespace` は**テナント内のパーティション**。テナント内の component 間ではセキュリティ境界
--    ではない（同一 owner = 同一信頼ドメインという前提。Workers の namespace-id 分離より弱い点は
--    ドキュメントに明記）。将来 per-component の namespace 許可リストで境界化できる。
--  - PK は複合キー（BIGSERIAL 不使用 → 0004 の SEQUENCE GRANT 依存を増やさない、m7c と同思想）。
--  - 値は BYTEA（テキストもバイナリも保持）。TTL は expires_at（NULL=無期限）。読み取り時に期限切れを
--    除外し、遅延削除は後続のスイープに委ねる（MVP では読み取りフィルタのみ）。
CREATE TABLE IF NOT EXISTS kv_entries (
    tenant_id   TEXT        NOT NULL REFERENCES tenants(id),
    namespace   TEXT        NOT NULL,
    key         TEXT        NOT NULL,
    value       BYTEA       NOT NULL,
    expires_at  TIMESTAMPTZ,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, namespace, key)
);

-- list（prefix 走査）用。tenant+namespace 内をキー順に引く。
CREATE INDEX IF NOT EXISTS idx_kv_entries_ns_key
    ON kv_entries (tenant_id, namespace, key);

-- RLS（0004/0011 と同じ fail-closed パターン: current_setting の第2引数フォールバック無し）。
ALTER TABLE kv_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE kv_entries FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON kv_entries
    USING      (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

GRANT SELECT, INSERT, UPDATE, DELETE ON kv_entries TO faas_app;
