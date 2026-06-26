-- M6: 同期 Invoke + Cron + 外部イベントトリガー（仕様書 §11 / §15）。
--
-- 0001-0007 への ADD のみ（additive・新規テーブル＝backfill 不要）。sqlx migrate ランナー
-- （control-plane 起動時）が 0007 の後に適用する（BASELINE_VERSIONS には入れない＝MIGRATOR.run
-- が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0007 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: CREATE TABLE / FORCE RLS / CREATE POLICY / GRANT / REVOKE /
-- CREATE FUNCTION SECURITY DEFINER は所有権を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（M6 設計書 §3）:
--  - 3 表とも 0004/0005/0007 と同型: ENABLE + FORCE RLS、fail-closed tenant_isolation
--    （current_setting('app.tenant_id') の第 2 引数フォールバックなし＝未設定 GUC は ERROR）。
--  - cron_jobs / triggers は CRUD（スケジューラが next_fire_at を UPDATE、CRUD API が DELETE する）。
--  - trigger_deliveries は配送冪等の**追記専用台帳**: SELECT/INSERT のみ付与し UPDATE/DELETE は
--    剥奪する（配送済みの権威＝event の二重起動防止を改竄不能にする, usage_rollups の DELETE 非付与
--    と同じ追記思想）。
--  - 全テナント巡回（スケジューラの due スキャン）は RLS 下で set_tenant_guc 無しに走らせられないため、
--    reaper::list_active_tenant_ids と同型の SECURITY DEFINER 関数 cron_due_tenant_jobs() を定義し、
--    スケジューラはそれで対象（tenant_id, job_id）だけを引いてから、各テナントごとに
--    set_tenant_guc した tx で fire 本処理（RLS 下）を行う。

-- ===========================================================================
-- (A) cron_jobs: 定時起動の登録（§11 / §15 M6b）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS cron_jobs (
    id              TEXT PRIMARY KEY,                       -- cron_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL REFERENCES components(id),
    schedule        TEXT NOT NULL,                          -- cron 式（5 フィールド）
    input           JSONB NOT NULL DEFAULT 'null'::jsonb,   -- fire 時に Component へ渡す入力
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    next_fire_at    TIMESTAMPTZ NOT NULL,                   -- 常に UTC。スケジューラが前進させる
    last_fired_slot BIGINT,                                 -- 直近 fire の scheduled_slot（unix 秒・冪等補助）
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- スケジューラの due スキャン用（enabled な行を next_fire_at 昇順で引く）。
CREATE INDEX IF NOT EXISTS idx_cron_jobs_due
    ON cron_jobs (next_fire_at) WHERE enabled;

-- ===========================================================================
-- (B) triggers: 外部イベント / component chain の登録（§11 / §15 M6c）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS triggers (
    id              TEXT PRIMARY KEY,                       -- trg_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL REFERENCES components(id), -- 起動対象（downstream）
    trigger_type    TEXT NOT NULL,                          -- 'object_storage' | 'chain'
    -- object_storage: {"bucket_prefix": "...", "events": ["put"]}
    -- chain:          {"source_component_id": "cmp_...", "on_status": "succeeded"}
    match_config    JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- payload→input マッピング指示（JSON Pointer ベース。未指定なら event payload を素通し）。
    input_mapping   JSONB,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- chain 解決 / event ルーティングはテナント内の enabled な trigger を引いてアプリ側で照合する
-- （match_config からの式 index は避ける）。
CREATE INDEX IF NOT EXISTS idx_triggers_enabled
    ON triggers (tenant_id) WHERE enabled;

-- ===========================================================================
-- (C) trigger_deliveries: イベント配送の重複排除台帳（追記専用, §15 M6c）。
--     event_dedup_id を PK に含め、同一イベント再送で 23505 → 既配送として無視する。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS trigger_deliveries (
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    trigger_id      TEXT NOT NULL,
    event_dedup_id  TEXT NOT NULL,                          -- 外部イベントの安定識別子
    execution_id    TEXT NOT NULL,                          -- enqueue した execution
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, trigger_id, event_dedup_id)     -- 配送冪等の権威
);

-- ===========================================================================
-- RLS（0007 usage_rollups パターン: fail-closed = current_setting の第 2 引数フォールバックなし →
-- 未設定 GUC は ERROR）。CREATE POLICY は PG15 未満で IF NOT EXISTS 不可だが、sqlx は
-- _sqlx_migrations で 1 度だけ適用するため可。
-- ===========================================================================
ALTER TABLE cron_jobs          ENABLE ROW LEVEL SECURITY;
ALTER TABLE cron_jobs          FORCE  ROW LEVEL SECURITY;
ALTER TABLE triggers           ENABLE ROW LEVEL SECURITY;
ALTER TABLE triggers           FORCE  ROW LEVEL SECURITY;
ALTER TABLE trigger_deliveries ENABLE ROW LEVEL SECURITY;
ALTER TABLE trigger_deliveries FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON cron_jobs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON triggers
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON trigger_deliveries
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- ===========================================================================
-- 権限。cron_jobs / triggers は CRUD（CRUD API の DELETE + scheduler の UPDATE）。
-- trigger_deliveries は配送台帳: 追記専用（DELETE/UPDATE 不可で重複排除の権威を改竄不能にする）。
-- ===========================================================================
REVOKE ALL                            ON cron_jobs          FROM PUBLIC;
REVOKE ALL                            ON triggers           FROM PUBLIC;
REVOKE ALL                            ON trigger_deliveries FROM PUBLIC;

GRANT  SELECT, INSERT, UPDATE, DELETE ON cron_jobs          TO   faas_app;
GRANT  SELECT, INSERT, UPDATE, DELETE ON triggers           TO   faas_app;

-- 配送台帳は SELECT/INSERT のみ。UPDATE/DELETE は防御的に REVOKE する（付与しないことが本質的保証）。
GRANT  SELECT, INSERT                 ON trigger_deliveries TO   faas_app;
REVOKE UPDATE, DELETE                 ON trigger_deliveries FROM faas_app;

-- ===========================================================================
-- 全テナント巡回用 SECURITY DEFINER 関数（スケジューラの due スキャン）。
-- テーブル所有者が所有し（definer != faas_app）、EXECUTE は faas_app に付与する。所有者として
-- 実行されるため faas_app の FORCE RLS の対象外で、GUC 未設定のまま全テナント横断で due 行の
-- (tenant_id, job_id) だけを返す（reaper の認証前参照と同型）。fire 本処理は呼び出し側が
-- 各テナントごとに set_tenant_guc した tx で RLS 下で行う。
-- ===========================================================================
CREATE FUNCTION cron_due_tenant_jobs()
    RETURNS TABLE (tenant_id text, job_id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT c.tenant_id, c.id
        FROM cron_jobs c
        JOIN tenants t ON t.id = c.tenant_id AND t.status = 'active'
        WHERE c.enabled AND c.next_fire_at <= now()
    $$;

REVOKE EXECUTE ON FUNCTION cron_due_tenant_jobs() FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION cron_due_tenant_jobs() TO   faas_app;

-- ===========================================================================
-- (E) executions.chain_depth: Component チェーンのホップ深さ（§15 M6c, 暴走防止）。
-- ===========================================================================
-- chain トリガーは「上流が succeeded → 下流を新規 execution として起動」する。下流は毎回 **新しい
-- execution_id** を持つため、配送台帳（trigger_deliveries）の冪等キー（上流 execution_id 基準）だけでは
-- A→B→A のような循環チェーンを止められない（各ホップが別 execution_id ＝ 別配送キーになり自己増殖する）。
-- そこで各 execution に「何ホップ目か」を持たせ、上流深さ+1 が MAX_CHAIN_DEPTH を超える起動を拒否する
-- （handlers::MAX_CHAIN_DEPTH）。HTTP / Cron / Object Storage 起点は root＝0、chain 下流のみ +1 する。
-- additive・DEFAULT 0 なので既存行 backfill 不要（0 = チェーン非関与/root と解釈できる）。
ALTER TABLE executions ADD COLUMN IF NOT EXISTS chain_depth INTEGER NOT NULL DEFAULT 0;
