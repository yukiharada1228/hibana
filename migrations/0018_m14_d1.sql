-- M14: D1 バインディング（Cloudflare Workers D1 互換, SQLite）のストア。
--
-- D1 は **隔離した実 SQLite** を (tenant, name) ごとに持つ。DB ファイル本体は bytea で保持し、
-- worker が実行の間だけメモリに常駐させて操作する（load 1回 / save 1回 / 実行終了時）。
-- 単一書き手は **Postgres の session advisory lock**（key = hash(tenant,name)）で保証する
-- ——同じ DB を同時に触る実行は直列化される。テナント境界は RLS。
--
-- **安全性**: guest の任意 SQL は worker 側の rusqlite authorizer で ATTACH/load_extension を
-- 拒否し、単一 SQLite ファイルの外へ出られないようにする（Postgres には一切触れない別エンジン）。
CREATE TABLE IF NOT EXISTS d1_databases (
    tenant_id  TEXT        NOT NULL REFERENCES tenants(id),
    name       TEXT        NOT NULL,
    data       BYTEA       NOT NULL,               -- SQLite ファイル本体（空 = 未初期化）
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name)
);

ALTER TABLE d1_databases ENABLE ROW LEVEL SECURITY;
ALTER TABLE d1_databases FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON d1_databases
    USING      (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

GRANT SELECT, INSERT, UPDATE, DELETE ON d1_databases TO faas_app;
