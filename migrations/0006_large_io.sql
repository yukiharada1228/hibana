-- M3d: 大容量 I/O 退避（input_ref / output_ref）+ テナント別クォータ（仕様書 §3.4 / §5.2 / §6.4 / §8 / §15）。
--
-- 0001-0005 への ADD のみ（additive・nullable 列／既定値付き＝backfill 不要）。sqlx migrate
-- ランナー（control-plane 起動時）が 0005 の後に適用する（BASELINE_VERSIONS には入れない＝
-- MIGRATOR.run が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0005 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: ALTER TABLE はテーブル所有権を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（USER-CONFIRMED M3d DECISIONS）:
--  - 大入力／大出力は Object Storage に退避し、executions.input_ref / output_ref（オブジェクトキー）で
--    参照する（§3.4 / §6.4。インライン上限 256 KiB 超）。両列とも nullable TEXT（無退避＝インライン invoke は NULL）。
--  - input_ref は `tenants/{tenant_id}/io/{execution_id}/input` に完全一致する値のみ保存される
--    （Axum が invoke 受付時に完全一致を検証してから INSERT する。prefix 一致は不可, §3.4 MUST）。
--  - tenants.quotas（JSONB, 既定 '{}'）に per-tenant の rate / concurrency クォータ上書きを格納する（§8）。
--    既定 '{}' は「グローバル既定を使う」を意味する（admission のグローバル既定 → テナント上書きの優先順位）。

-- 1. executions: 大容量 I/O の退避参照（全て nullable → additive・backfill 不要, §3.4 / §6.4）。
--    executions は 0004_rls.sql の FORCE RLS + tenant_isolation 下にあり、列追加はその対象テーブルへの
--    ADD COLUMN なので RLS ポリシー（USING/WITH CHECK = tenant_id 一致）はそのまま全列に適用される
--    （列単位の RLS ではないため追加ポリシー定義は不要; 0004 と整合）。
ALTER TABLE executions ADD COLUMN IF NOT EXISTS input_ref  TEXT;   -- 大入力の Object Storage キー（NULL=インライン）
ALTER TABLE executions ADD COLUMN IF NOT EXISTS output_ref TEXT;   -- 大出力の Object Storage キー（NULL=インライン）

-- 2. tenants: per-tenant クォータ上書き（§8）。tenants は tenant_id 列を持たず 0004 の RLS 対象外
--    （tenant 自身を表す行であり、参照は SECURITY DEFINER 関数 or RLS 無しの直接 SELECT）。
--    既定 '{}' = 上書き無し（admission はグローバル既定を使う）。例:
--      {"invoke_rate_per_sec": 100, "invoke_burst": 1000, "max_concurrent_executions": 50}
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS quotas JSONB NOT NULL DEFAULT '{}'::jsonb;
