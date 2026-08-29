-- M7a: バージョン traffic splitting / canary + 即時 rollback（仕様書 §6.7 / §15）。
--
-- 0001-0008 への ADD のみ（additive。追加列は NOT NULL + DEFAULT か nullable ＝ backfill 不要）。
-- sqlx migrate ランナー（control-plane 起動時）が 0008 の後に適用する（BASELINE_VERSIONS には
-- 入れない＝MIGRATOR.run が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0008 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: ALTER TABLE / ADD CONSTRAINT / CREATE INDEX は所有権を要する
-- （ランタイムの faas_app では実行できない）。
--
-- ローリング更新時のロック窓（重要・運用注記）:
--   executions は本システムで最も行数が伸びる表であり、以下の 2 文が書き込みを一時的にブロックする。
--     (1) ALTER TABLE executions ADD COLUMN routing_reason ... NOT NULL DEFAULT 'stable'
--         → PG11+ は既定値をカタログに持つのでテーブル書き換えは起きない（メタデータのみ・短時間の
--           ACCESS EXCLUSIVE）。**インライン CHECK は書かない**（既存全行の検証走査を誘発するため。
--           値域は下の NOT VALID 制約 + Rust 側 RoutingReason の 2 値で担保する）。
--     (2) CREATE INDEX idx_executions_component_finished（CONCURRENTLY 不可: sqlx は各 migration を
--         1 tx で走らせる）→ 索引構築のあいだ ACCESS EXCLUSIVE を取る。行数に比例した停止時間になる。
--   本番では `--migrate-only` 経路（control-plane の起動引数）で保守窓に単独適用してから
--   ローリング更新すること。README トラブルシュートに手順を書く。
--
-- 設計（M7 設計書 §1.1-1.3）:
--  - **新規テーブルを作らない**。canary の配分は components への additive 列で持つ。components は
--    0004_rls.sql で ENABLE+FORCE RLS + fail-closed tenant_isolation 済みであり、RLS は列単位ではなく
--    行単位なので ADD COLUMN には既存ポリシーがそのまま全列に適用される（0006/0007 の ADD COLUMN と
--    同じ整合性根拠）。0004_rls.sql の GRANT も components を列挙済みのため追加 GRANT も不要。
--    → 「新テーブルの GRANT 漏れ / RLS ポリシー漏れ」という M7 最大の退行リスクを構造的に消す。
--  - ルーティングの権威は components の 2 ポインタ（active_version_id=stable / canary_version_id）と
--    canary_weight（0-100 の整数パーセント）。invoke はこの 1 行を LEFT JOIN 1 本で読む（追加クエリ 0）。
--  - canary_version_id には FK を張らない。active_version_id が FK なしの素の TEXT（0001_init.sql）
--    であることとの対称性を保つため、かつ FK では防げない soft delete（component_versions.deleted_at,
--    0002_m2.sql）を解決 SQL の JOIN 条件で fail-safe に倒すほうが強い保証になるため。参照存在検証は
--    アプリ側（db::find_version_id）で必ず行う。
--  - executions.routing_reason は「その実行がどちら側で選ばれたか」の不変な記録。components は可変
--    なので version_id だけでは昇格後に stable/canary の区別が失われる（0008_m6.sql の chain_depth と
--    同じ additive + NOT NULL DEFAULT の判断: 既存行は 'stable' が意味的に正しく backfill 不要）。

-- ===========================================================================
-- (A) components: canary 配分と rollback 用の直前 stable（§6.7 / §15 M7a）。
-- ===========================================================================
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_version_id TEXT;                    -- canary 側の version（FK なし: 上記）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_weight SMALLINT NOT NULL DEFAULT 0; -- canary へ流す割合（%）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS previous_active_version_id TEXT;           -- 直前の stable（引数なし rollback 用）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_updated_at TIMESTAMPTZ;             -- 最終ルーティング変更

-- 値域と「配分先の無い重み」を DB で不可能にする。ALTER TABLE ... ADD CONSTRAINT は IF NOT EXISTS を
-- 書けず単体では非冪等だが、sqlx は _sqlx_migrations で 1 度だけ適用するため可
-- （0007_usage_metering.sql / 0008_m6.sql と同じ正当化。手動再実行は前提にしない）。
-- components は行数が小さい（component 数）ので即時 VALIDATE してよい（executions とは扱いが違う）。
ALTER TABLE components
    ADD CONSTRAINT components_canary_weight_range
        CHECK (canary_weight >= 0 AND canary_weight <= 100);
ALTER TABLE components
    ADD CONSTRAINT components_canary_weight_requires_version
        CHECK (canary_weight = 0 OR canary_version_id IS NOT NULL);

-- M7b/M7c の複合 FK（tenant 一致を DB で強制する）の被参照側。既存 PK (id) は維持したまま
-- (tenant_id, id) の一意性を足す（additive）。0010/0011 が
-- FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id) でこれを参照する。
ALTER TABLE components
    ADD CONSTRAINT components_tenant_id_id_key UNIQUE (tenant_id, id);

-- 進行中の canary を運用側から引く（件数が小さいので部分 index）。
CREATE INDEX IF NOT EXISTS idx_components_canary_active
    ON components (tenant_id) WHERE canary_weight > 0;

-- ===========================================================================
-- (B) executions.routing_reason: version 決定理由（§15 M7a の観測）。
-- ===========================================================================
-- version_id だけでは「昇格後に stable になった版が canary として選ばれた実行」を後から区別できない
-- （components は可変で、実行時点のスナップショットではない）。NOT NULL + DEFAULT 'stable' なので
-- 既存行の backfill は不要（canary 導入前の全実行は定義上 stable 相当）。
-- **インライン CHECK は書かない**（上のロック窓注記）。値域は NOT VALID 制約で前方だけ守る。
ALTER TABLE executions
    ADD COLUMN IF NOT EXISTS routing_reason TEXT NOT NULL DEFAULT 'stable';

-- NOT VALID: 既存行のフルスキャン検証を行わず、以後の INSERT/UPDATE にだけ効かせる。
-- 既存行は全て DEFAULT 'stable' なので実質的に既に適合しており、保守窓で
--   ALTER TABLE executions VALIDATE CONSTRAINT executions_routing_reason_chk;
-- を手で流せば完全化できる（README 運用手順）。
ALTER TABLE executions
    ADD CONSTRAINT executions_routing_reason_chk
        CHECK (routing_reason IN ('stable', 'canary')) NOT VALID;

-- canary 判断（version 別の成功率・レイテンシ）用の直近ウィンドウ走査。既存 index は
-- (tenant_id, created_at DESC) / (component_id, status)（0001_init.sql）と
-- (tenant_id, finished_at DESC) 部分 index（0007_usage_metering.sql）で、component を絞った
-- 終端行の時間窓走査に効かない。追加のみで既存クエリの計画は変えない。
CREATE INDEX IF NOT EXISTS idx_executions_component_finished
    ON executions (tenant_id, component_id, finished_at DESC)
    WHERE status IN ('succeeded', 'failed', 'timeout');

-- ===========================================================================
-- RLS / 権限について（明示）: 本マイグレーションは新規テーブルを作らないため、
-- CREATE POLICY / ENABLE|FORCE RLS / GRANT / REVOKE は一切不要である。
--  - components は 0004_rls.sql で ENABLE+FORCE RLS + fail-closed tenant_isolation 済み。
--  - executions も同じ（0004_rls.sql）。行単位ポリシーは追加列にもそのまま適用される。
--  - faas_app への GRANT も 0004_rls.sql が両テーブルを列挙済み（列単位 GRANT ではない）。
-- ===========================================================================
