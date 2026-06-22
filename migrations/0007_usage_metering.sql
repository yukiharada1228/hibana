-- M5: per-execution 計量 + テナント別期間集計（usage_rollups）+ 利用量参照（仕様書 §6.0 / §15）。
--
-- 0001-0006 への ADD のみ（additive・nullable 列 / 既定値付き新規テーブル＝backfill 不要）。sqlx migrate
-- ランナー（control-plane 起動時）が 0006 の後に適用する（BASELINE_VERSIONS には入れない＝
-- MIGRATOR.run が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0006 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: ALTER TABLE / CREATE TABLE ... FORCE RLS / CREATE POLICY / GRANT / REVOKE は
-- 所有権を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（USER-CONFIRMED M5 DECISIONS）:
--  - 冪等の唯一のアンカーは既存の CAS finalize（pending/running → 終端の UPDATE）。executions の計量列は
--    その finalize UPDATE の SET 句に同梱され（s2/s4）、再配送・worker 多重実行・DLQ 後着は既終端行に
--    当たって rows_affected==0 となり計量も同時に no-op になる（status と計量が同一行・同一述語で原子更新）。
--  - 計量列は全て nullable: DLQ / timeout / 未計測 result では計量欠落が正常であり、0（計測して 0）と
--    NULL（未計測）を区別したいため（0006 の input_ref/output_ref と同型の additive 規約）。
--  - usage_rollups はテナント×UTC日×component 粒度の事前集計。finalize と同一 tx 内で increment UPSERT
--    する（s4）。peak_memory は SUM ではなく MAX セマンティクス（列名 _max で明示）。
--    DELETE は付与しない＝集計の改竄/消去を不可にする（請求の権威, §3.2 と同じ追記思想）。
--  - sweeper(reaper) で終端化された stuck execution は usage_rollups に反映しない（バッチかつ計量を持たない）。
--    invocation_count の合計が executions の終端行数と稀に一致しないことがある＝既知の許容差（運用ドキュメント記載）。

-- 1. executions: per-execution 計量列（全て nullable → additive・backfill 不要, §15 M5）。
--    executions は 0004_rls.sql の FORCE RLS + tenant_isolation 下にあり、列追加はその対象テーブルへの
--    ADD COLUMN なので RLS ポリシー（USING/WITH CHECK = tenant_id 一致）はそのまま全列に適用される
--    （列単位の RLS ではないため追加ポリシー定義は不要; 0006 コメントと同じ整合性根拠）。
--    NULL=未計測（DLQ/timeout/未計測 result）, 値=計測済（succeeded result 等）。
ALTER TABLE executions ADD COLUMN IF NOT EXISTS cpu_fuel_used     BIGINT;   -- 消費 fuel（= 設定値 - 残 fuel。fuel 無効化時は 0）
ALTER TABLE executions ADD COLUMN IF NOT EXISTS wall_time_ms      BIGINT;   -- 実行 wall time（ミリ秒）
ALTER TABLE executions ADD COLUMN IF NOT EXISTS peak_memory_bytes BIGINT;   -- ピークメモリ（StoreLimits 経由。未取得時は 0）
ALTER TABLE executions ADD COLUMN IF NOT EXISTS output_bytes      BIGINT;   -- 出力バイト数（出力エンコード後）
ALTER TABLE executions ADD COLUMN IF NOT EXISTS invocation_count  INTEGER;  -- 呼び出し回数（終端化された 1 実行 = 1）

-- 2. テナント別期間集計テーブル（事前集計・GET /usage が直読みする, §6.0）。
--    粒度はテナント×UTC日（period_start: DATE）×component。日境界はサーバ UTC 固定（課金粒度として仕様で固定）。
--    複合 PRIMARY KEY が s4 の increment UPSERT の競合ターゲット（ON CONFLICT (tenant_id, period_start, component_id)）。
--    invocation_count は全終端（succeeded/failed/timeout）で +1、リソース指標は計測済み result のみ加算する
--    （DLQ/timeout は failed_count/timeout_count を +1 しつつ cpu/wall/output は 0 加算＝半端行; 解釈は API ドキュメントで明示）。
CREATE TABLE IF NOT EXISTS usage_rollups (
    tenant_id             TEXT NOT NULL REFERENCES tenants(id),
    period_start          DATE NOT NULL,                              -- UTC 日境界（課金粒度）
    component_id          TEXT NOT NULL REFERENCES components(id),
    invocation_count      BIGINT NOT NULL DEFAULT 0,                  -- 全終端の呼び出し回数
    cpu_fuel_used         BIGINT NOT NULL DEFAULT 0,                  -- 消費 fuel 合計（SUM）
    wall_time_ms          BIGINT NOT NULL DEFAULT 0,                  -- wall time 合計（SUM, ミリ秒）
    peak_memory_bytes_max BIGINT NOT NULL DEFAULT 0,                  -- ピークメモリの最大値（MAX セマンティクス）
    output_bytes          BIGINT NOT NULL DEFAULT 0,                  -- 出力バイト数合計（SUM）
    succeeded_count       BIGINT NOT NULL DEFAULT 0,                  -- 終端 succeeded 件数
    failed_count          BIGINT NOT NULL DEFAULT 0,                  -- 終端 failed 件数（DLQ 含む）
    timeout_count         BIGINT NOT NULL DEFAULT 0,                  -- 終端 timeout 件数
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, period_start, component_id)
);

-- 3. RLS（0005 audit_logs パターンを踏襲: fail-closed = current_setting の第 2 引数フォールバックなし →
--    未設定 GUC は ERROR）。CREATE POLICY は PG15 未満で IF NOT EXISTS 不可だが、sqlx は
--    _sqlx_migrations で 1 度だけ適用するため可。
ALTER TABLE usage_rollups ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_rollups FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON usage_rollups
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- 4. 権限（rollup は increment UPSERT で加算するため UPDATE が必要。DELETE は付与しない＝改竄/消去不可）。
--    PUBLIC から明示剥奪し、faas_app には SELECT/INSERT/UPDATE のみ。DELETE は防御的に REVOKE する。
--    シーケンス無し（BIGSERIAL を使わず複合 PK のため SEQUENCE GRANT 不要）。
REVOKE ALL                    ON usage_rollups FROM PUBLIC;
GRANT  SELECT, INSERT, UPDATE ON usage_rollups TO   faas_app;
REVOKE DELETE                 ON usage_rollups FROM faas_app;        -- 防御的: 付与しないことが本質的保証

-- 5. 参照用インデックス。
--    rollup の期間範囲 SELECT（GET /usage が tenant_id でフィルタし period_start 降順で読む）。
CREATE INDEX IF NOT EXISTS idx_usage_rollups_tenant_period
    ON usage_rollups (tenant_id, period_start DESC);
--    生計量の期間集計直読み用（終端行のみの部分インデックス）。
CREATE INDEX IF NOT EXISTS idx_executions_tenant_finished
    ON executions (tenant_id, finished_at DESC)
    WHERE status IN ('succeeded', 'failed', 'timeout');
