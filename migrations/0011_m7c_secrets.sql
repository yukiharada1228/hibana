-- M7c: Secrets Manager（仕様書 §10 / §15）。
--
-- 0001-0010 への ADD のみ（新規テーブル＝backfill 不要）。sqlx migrate ランナー
-- （control-plane 起動時）が 0010 の後に適用する（BASELINE_VERSIONS には入れない＝MIGRATOR.run
-- が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0010 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: CREATE TABLE / FORCE RLS / CREATE POLICY / GRANT / REVOKE /
-- CREATE FUNCTION SECURITY DEFINER は所有権を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（M7 設計書 §1.8 / §4）:
--  - **BIGSERIAL を使わない**。PK は複合キー。→ 0004_rls.sql の GRANT ... ON ALL SEQUENCES
--    （0004 適用時点のシーケンスにしか効かない）や 0005_provenance.sql の明示 SEQUENCE GRANT を
--    必要としない。退行の起きやすい GRANT 漏れを一つ減らす。
--  - 値は **component 単位**（version には紐づけない）。よって component_versions への FK は張らない。
--    「どの version から読めるか」は capabilities.env（admin 承認の許可リスト）が決める。
--  - 複合 FK でテナント一致を DB 側でも強制する（0010 と同じ理由: RLS の WITH CHECK は
--    「自分の tenant_id を書くこと」しか要求しないため、単一列 FK ではテナントを跨いだ紐づけを防げない）。
--  - **版台帳は追記専用**。値の変更（rotate）も KEK の再ラップ（rekey）も「新しい version 行の
--    INSERT」で表現する → UPDATE を一切 GRANT しないまま rotation が成立する
--    （audit_logs / trigger_deliveries と同じ追記思想）。

-- ===========================================================================
-- (A) function_secrets: メタデータ + 現行世代ポインタ。
--     **平文も暗号文もこの表には無い**（暗号材料は (B) の版台帳だけが持つ）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_secrets (
    id              TEXT PRIMARY KEY,                -- sec_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL,
    name            TEXT NOT NULL,                   -- 注入される env キー名と同一（別名を作らない）
    current_version INTEGER NOT NULL,                -- 注入対象の世代（versions.version を指す）
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ,                     -- soft delete（§6.7 と同思想）
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id),
    -- 0011 の中で自己参照される複合キー（versions 側の複合 FK 用）。
    CONSTRAINT function_secrets_tenant_id_id_key UNIQUE (tenant_id, id)
);

-- name の一意性は **生存行のみ**（部分 UNIQUE index）。
-- テーブル制約 UNIQUE (tenant_id, component_id, name) にすると、soft delete 後に同名で作り直せず
-- 23505 になる。「侵害された資格情報を削除して同名で入れ直す」というインシデント対応が最も基本の
-- 操作であり、これを不可能にしてはならない。0005_provenance.sql の部分 UNIQUE
-- (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL と同じ作法。
CREATE UNIQUE INDEX IF NOT EXISTS uq_function_secrets_live_name
    ON function_secrets (tenant_id, component_id, name) WHERE deleted_at IS NULL;

-- 実行のたびに (tenant, component) の生存 secret を引く（注入経路）。
CREATE INDEX IF NOT EXISTS idx_function_secrets_component
    ON function_secrets (tenant_id, component_id) WHERE deleted_at IS NULL;

-- ===========================================================================
-- (B) function_secret_versions: 封筒暗号の版台帳（**追記専用**）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_secret_versions (
    tenant_id    TEXT    NOT NULL REFERENCES tenants(id),
    secret_id    TEXT    NOT NULL,
    version      INTEGER NOT NULL,                   -- 1 から単調増加
    kek_kid      TEXT    NOT NULL,                   -- この行を包んだ KEK の kid（signing.rs と同概念）
    wrapped_dek  BYTEA   NOT NULL,                   -- AEAD(KEK, dek_nonce, DEK, aad=dek_aad)
    dek_nonce    BYTEA   NOT NULL,                   -- 24 バイト（XChaCha20-Poly1305）
    nonce        BYTEA   NOT NULL,                   -- 24 バイト
    ciphertext   BYTEA   NOT NULL,                   -- AEAD(DEK, nonce, plaintext, aad=value_aad)
    value_len    INTEGER NOT NULL,                   -- 平文バイト長（**API へは出さない**, §5.4）
    reason       TEXT    NOT NULL,                   -- 'create' | 'rotate' | 'rekey'
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by   TEXT,                               -- 作成した principal の user_id（監査補助）
    PRIMARY KEY (tenant_id, secret_id, version),
    FOREIGN KEY (tenant_id, secret_id) REFERENCES function_secrets (tenant_id, id),
    CONSTRAINT function_secret_versions_reason_chk
        CHECK (reason IN ('create', 'rotate', 'rekey'))
);

-- execution 基準の世代解決（§4.7）で使う: 指定 secret の created_at <= T な最大 version。
-- 「同一 execution の再配送は同一世代を返す」という不変条件がこの索引に乗る。
CREATE INDEX IF NOT EXISTS idx_function_secret_versions_created
    ON function_secret_versions (tenant_id, secret_id, created_at DESC);

-- ===========================================================================
-- RLS + 権限（0007/0008 パターン: fail-closed ＝ current_setting の第 2 引数フォールバックなし）
-- ===========================================================================
ALTER TABLE function_secrets         ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secrets         FORCE  ROW LEVEL SECURITY;
ALTER TABLE function_secret_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secret_versions FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON function_secrets
    USING       (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK  (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON function_secret_versions
    USING       (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK  (tenant_id = current_setting('app.tenant_id'));

REVOKE ALL                    ON function_secrets         FROM PUBLIC;
REVOKE ALL                    ON function_secret_versions FROM PUBLIC;

-- function_secrets は current_version の前進 / soft delete のため UPDATE が要る。DELETE は不要
-- （削除は soft delete。物理削除を faas_app にさせない）。
GRANT  SELECT, INSERT, UPDATE ON function_secrets         TO   faas_app;
REVOKE DELETE                 ON function_secrets         FROM faas_app;
-- 版台帳は追記専用（暗号文の改竄・消去を faas_app から不可能にする）。
GRANT  SELECT, INSERT         ON function_secret_versions TO   faas_app;
REVOKE UPDATE, DELETE         ON function_secret_versions FROM faas_app;

-- ===========================================================================
-- SECURITY DEFINER 関数（0008_m6.sql の cron_due_tenant_jobs() と同型）。
--
-- **重大な設計上の区別**: 本リポジトリの `admin` は **テナント管理者**であってプラットフォーム
-- 管理者ではない（admin_routes は auth::authenticate 配下で principal.tenant_id が権威。
-- プラットフォーム唯一の非テナント面は POST /admin/tenants のみ）。
-- したがって **HTTP ハンドラから呼ぶ関数は必ず p_tenant を取る**。全テナント横断版は
-- `_all` 接尾辞を付け、**CP 内部の背景ジョブ（scheduler.rs / reaper.rs 型）からのみ**呼ぶ。
-- _all の結果を HTTP 応答に載せてはならない (MUST NOT)。
--
-- SECURITY DEFINER である理由は cron_due_tenant_jobs() と同じ: faas_app は FORCE RLS 下にあり
-- set_tenant_guc なしに巡回できない。返す列は (tenant_id, secret_id) / (kid, count) に絞り、
-- **暗号文も secret 名も返さない**（0004_rls.sql の認証前参照 3 関数が「返す列は必要最小限」と
-- している作法）。再ラップの本処理は呼び出し側が各テナントごとに set_tenant_guc した tx で
-- RLS 下に行う。
-- ===========================================================================

-- (a) テナント内の、現行 kid でない current 世代を持つ secret（HTTP rekey が使う）。
CREATE FUNCTION secrets_stale_kek(p_tenant text, p_active_kid text)
    RETURNS TABLE (tenant_id text, secret_id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT s.tenant_id, s.id
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.tenant_id = p_tenant AND s.deleted_at IS NULL AND v.kek_kid <> p_active_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_stale_kek(text, text) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_stale_kek(text, text) TO   faas_app;

-- (b) 全テナント巡回版（**背景ジョブ専用**。HTTP からは呼ばない）。
CREATE FUNCTION secrets_stale_kek_all(p_active_kid text)
    RETURNS TABLE (tenant_id text, secret_id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT s.tenant_id, s.id
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        JOIN tenants t ON t.id = s.tenant_id AND t.status = 'active'
        WHERE s.deleted_at IS NULL AND v.kek_kid <> p_active_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_stale_kek_all(text) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_stale_kek_all(text) TO   faas_app;

-- (c) kid 別の current 世代件数（**Prometheus gauge の内部更新専用**。HTTP 応答には載せない）。
CREATE FUNCTION secrets_kek_kid_counts_all()
    RETURNS TABLE (kek_kid text, n bigint)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT v.kek_kid, count(*)
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.deleted_at IS NULL
        GROUP BY v.kek_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_kek_kid_counts_all() FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_kek_kid_counts_all() TO   faas_app;
