-- M2: アップロード→検証→デプロイ→invoke を支える追加スキーマ。
-- 仕様書 §3.2（メタデータ）/ §6.2（アップロード+検証パイプライン）/ §6.7（バージョン管理API）。
--
-- 0001 (M1) への ADD のみ。一度だけ適用する前提（冪等性は不要）。
-- 原則(§15): RLS / テナント分離は M3 で一括導入する。M2 は単一テナント 'default' 固定・RLS 無し。

-- §6.2: 検証パイプラインで確定した本体サイズと許可済み import/capability を記録する。
-- §6.7: バージョンの soft delete（active_version_id からの解決対象から外す）。
ALTER TABLE component_versions
    ADD COLUMN size_bytes   BIGINT      NOT NULL DEFAULT 0,            -- §6.2: 本体サイズ（上限チェック後の確定値）
    ADD COLUMN capabilities JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- §4.4: 承認済み import/capability の記録
    ADD COLUMN deleted_at   TIMESTAMPTZ;                              -- §6.7: バージョン soft delete

-- §6.7: latest 解決・一覧（component ごとに新しい順）を引くための補助 index。
CREATE INDEX idx_component_versions_component ON component_versions (component_id, created_at DESC);
