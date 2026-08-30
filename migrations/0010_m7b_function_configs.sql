-- M7b: per-function の平文環境変数（仕様書 §15 M7 / §4.4）。
--
-- 0001-0009 への ADD のみ（新規テーブル＝backfill 不要）。sqlx migrate ランナー
-- （control-plane 起動時）が 0009 の後に適用する（BASELINE_VERSIONS には入れない＝MIGRATOR.run
-- が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0009 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: CREATE TABLE / FORCE RLS / CREATE POLICY / GRANT / REVOKE は所有権を要する
-- （ランタイムの faas_app では実行できない）。
--
-- 設計（M7 設計書 §1.5-1.7）:
--  - **config（平文）と secret（暗号化）を分ける**。同じ表に「暗号化フラグ」を持たせると、
--    読み出し API・監査・ログの各分岐で「今これは平文か」を毎回判定することになり、1 箇所の
--    分岐漏れが即漏洩になる。表を分ければ「function_configs は読める / function_secrets は
--    読めない」が型と権限の構造そのものになる。
--  - **行 = 1 キー**（JSONB 1 列にしない）。JSONB だと serde_json::Value 経由で API 応答や
--    audit_logs.detail へ丸ごと横流れする経路ができる。行単位ならキーが監査 target になり、
--    値が Value として飛び回らない。
--  - **複合 FK (tenant_id, component_id) -> components (tenant_id, id)**: 単一列 FK
--    （components(id)）はテナント一致を強制しない。RLS の WITH CHECK は「自分の tenant_id を
--    書くこと」しか要求しないため、テナント A が「tenant_id=A, component_id=（B の cmp_*）」という
--    行を作れてしまう。0009 が足した components (tenant_id, id) UNIQUE を参照し、DB 側でも
--    同一テナント内であることを保証する（RLS + WHERE + FK の三重防御）。

-- ===========================================================================
-- (A) function_configs: per-function の平文環境変数（§15 M7b）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_configs (
    tenant_id    TEXT NOT NULL REFERENCES tenants(id),
    component_id TEXT NOT NULL,
    key          TEXT NOT NULL,                      -- ^[A-Z_][A-Z0-9_]{0,63}$（CP が検証）
    value        TEXT NOT NULL,                      -- 平文。読み出し可（secret ではない）
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by   TEXT,                               -- 更新した principal の user_id（監査補助）
    PRIMARY KEY (tenant_id, component_id, key),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)
);

-- worker が実行のたびに (tenant, component) 単位で全キーを引くため、PK 前方一致で足りる
-- （PRIMARY KEY (tenant_id, component_id, key) の左 2 列）。追加 index は置かない。

-- ===========================================================================
-- RLS（0007/0008 パターン: fail-closed ＝ current_setting の第 2 引数フォールバックなし
-- → 未設定 GUC は ERROR）。CREATE POLICY は IF NOT EXISTS を書けないが、sqlx は
-- _sqlx_migrations で 1 度だけ適用するため可（0008_m6.sql と同じ正当化）。
-- ===========================================================================
ALTER TABLE function_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_configs FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON function_configs
    USING       (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK  (tenant_id = current_setting('app.tenant_id'));

-- 権限。0004_rls.sql の GRANT はテーブル名の列挙であり新表を含まないため、ここで明示する
-- （書き忘れると RLS 以前に権限エラーで faas_app から一切触れない ＝ 新表追加時の最頻の退行）。
REVOKE ALL                            ON function_configs FROM PUBLIC;
GRANT  SELECT, INSERT, UPDATE, DELETE ON function_configs TO   faas_app;
