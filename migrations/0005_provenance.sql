-- M3c: 出所認証（偽造不能な結果）+ 冪等性 + 追記専用 audit ログ（仕様書 §3.3 / §6.6 / §3.7 / §15）。
--
-- 0001-0004 への ADD のみ（additive・nullable 列＝backfill 不要）。sqlx migrate ランナー
-- （control-plane 起動時）が 0004 の後に適用する（BASELINE_VERSIONS には入れない＝
-- MIGRATOR.run が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0004 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: ALTER TABLE ... FORCE RLS / CREATE POLICY / GRANT / REVOKE は
-- 所有権を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（USER-CONFIRMED M3c DECISIONS）:
--  - 冪等性は (tenant_id, idempotency_key) で dedup（部分 UNIQUE）。同一 key + 異なる body → 409、
--    同一 key + 同一 body → 既存 execution を返す（§6.6 / decision #4）。body hash は不変条件として保存する。
--  - 結果トークンの kid を観測用に記録する（rotation 観測; トークン本体は保存しない）。
--  - audit_logs は 0004 と同じ FORCE RLS + tenant_isolation で隔離し、追記専用にする
--    （faas_app は SELECT/INSERT のみ。UPDATE/DELETE は付与しない＝改竄・削除不可, §3.2）。

-- 1. executions: 冪等性 + トークン出所の列（全て nullable → additive・backfill 不要）。
ALTER TABLE executions ADD COLUMN IF NOT EXISTS idempotency_key          TEXT;
ALTER TABLE executions ADD COLUMN IF NOT EXISTS idempotency_request_hash TEXT;   -- canonical invoke body の sha256 hex
ALTER TABLE executions ADD COLUMN IF NOT EXISTS job_token_kid            TEXT;   -- 発行トークンに埋めた kid（rotation 観測）

-- 2. 冪等キーの一意制約（テナント名前空間・key を持つ行のみ。NULL は除外＝無キー invoke は無制約, §6.6）。
--    並行同一 key の二重 INSERT はこの index が権威（呼び出し側は 23505 を捕捉して再 SELECT する）。
CREATE UNIQUE INDEX IF NOT EXISTS uq_executions_tenant_idem
    ON executions (tenant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- 3. 追記専用 audit ログ（§3.2 / §3.7）。tenant_id を持ち 0004 と同じ RLS で隔離する。
CREATE TABLE IF NOT EXISTS audit_logs (
    id          BIGSERIAL PRIMARY KEY,            -- 追記専用（単調増加）
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    actor       TEXT,                              -- user_id / token_id / 'worker' / NULL
    action      TEXT NOT NULL,                     -- result_token_missing | result_token_verify_failed | result_tenant_mismatch | idempotency_conflict | ...
    target      TEXT,                              -- execution_id 等の対象 id
    detail      JSONB,                             -- 理由・id のみ（生トークン・秘密は記録しない）
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_audit_logs_tenant_created ON audit_logs (tenant_id, created_at DESC);

-- 4. RLS（0004 と一致: fail-closed = current_setting の第 2 引数フォールバックなし → 未設定 GUC は ERROR）。
--    CREATE POLICY は PG15 未満で IF NOT EXISTS 不可だが、sqlx は _sqlx_migrations で 1 度だけ適用するため可。
ALTER TABLE audit_logs ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_logs FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON audit_logs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- 5. 追記専用の権限（§3.2 MUST NOT mutate/delete）。faas_app は SELECT/INSERT のみ。
--    UPDATE/DELETE は付与しない（=既定で不可）。PUBLIC からも明示的に剥奪し、faas_app からも
--    防御的に REVOKE する（0004 は audit_logs 未作成のため UPDATE/DELETE を付与していないが、明示する）。
REVOKE ALL            ON audit_logs FROM PUBLIC;
GRANT  SELECT, INSERT ON audit_logs TO   faas_app;
REVOKE UPDATE, DELETE ON audit_logs FROM faas_app;             -- 防御的: 付与しないことが本質的保証
GRANT  USAGE, SELECT  ON SEQUENCE audit_logs_id_seq TO faas_app;  -- BIGSERIAL nextval（0004 の ALL SEQUENCES に加え明示）
