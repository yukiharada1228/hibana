-- M9a: 署名付き Component と供給網検証（仕様書 §15 M9 / §6.2）。
--
-- 0001-0011 への ADD のみ（新規テーブル + 既存表への ADD COLUMN、backfill 不要）。
-- sqlx migrate ランナー（control-plane 起動時）が 0011 の後に適用する
-- （BASELINE_VERSIONS には入れない）。**テーブル所有ロール**（MIGRATION_DATABASE_URL）で実行する。
--
-- 脅威（§15 M9 の T6）: deploy スコープのトークンが漏れると、攻撃者が任意の wasm を
-- active version にできる。署名検証はこれを塞ぐ —— 「アップロードの認可（deploy トークン）」と
-- 「本体の真正性（テナント所有の署名鍵）」を**別々の秘密**に依存させ、片方が漏れても攻撃が
-- 成立しないようにする。
--
-- 設計:
--  - 署名対象は wasm 本体そのものではなく **wasm_sha256**（検証パイプラインが既に算出済み）。
--    アップロードされた本体の sha256 == 署名が主張する sha256 + Ed25519 検証、の 2 段。
--    sha256 の衝突耐性への依存は §6.6 冪等性と cwasm キャッシュキーが既に置いている前提なので、
--    新たな仮定を増やさない。
--  - 鍵は**テナント所有**（job token 署名鍵とは完全に別ドメイン）。複数鍵を許しローテーション可能。
--  - enforcement は per-tenant のポリシー（tenants.require_signed_components）。既定 false は
--    「アップグレードで既存テナントのデプロイが突然壊れない」ため（M8 の lane / M9c の egress と同じ思想）。

-- ===========================================================================
-- (A) component_signing_keys: テナントが登録する Ed25519 公開鍵（§6.2）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS component_signing_keys (
    tenant_id  TEXT NOT NULL REFERENCES tenants(id),
    key_id     TEXT NOT NULL,                          -- テナント内で一意な鍵の識別子（ローテーション用）
    -- Ed25519 公開鍵（32 バイト）を base64url（パディング無し, 43 文字）で保持する。
    -- 秘密鍵は**プラットフォームに一切渡さない**（テナントが手元で署名する）。
    public_key TEXT NOT NULL,
    -- 'active'（新規署名の推奨鍵）/ 'retired'（検証は通すが推奨から外す）。
    -- M7c secret の KEK と同じ思想: 古い鍵で署名された既存 version も検証できるよう retired を残す。
    status     TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT,                                   -- 登録した principal の user_id（監査補助）
    PRIMARY KEY (tenant_id, key_id)
);

-- 検証は実行のたびに tenant の active/retired 鍵を全て引く（PK 前方一致で足りる）。追加 index 不要。

ALTER TABLE component_signing_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE component_signing_keys FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON component_signing_keys
    USING       (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK  (tenant_id = current_setting('app.tenant_id'));

-- 0004_rls.sql の GRANT は新表を含まないため明示する（書き忘れると faas_app から一切触れない）。
REVOKE ALL                            ON component_signing_keys FROM PUBLIC;
GRANT  SELECT, INSERT, UPDATE, DELETE ON component_signing_keys TO   faas_app;

-- ===========================================================================
-- (B) tenants.require_signed_components: 署名必須ポリシー（§15 M9）。
-- ===========================================================================
-- 既定 false = 従来どおり（署名が有れば検証はするが必須ではない）。true にしたテナントは
-- 署名が無い / 検証に失敗する version を active にできない（アップロードが 422）。
ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS require_signed_components BOOLEAN NOT NULL DEFAULT false;

-- faas_app は tenants に SELECT/INSERT だけを持つ（0004_rls.sql）。ポリシー切替のために
-- **この 1 列だけ** UPDATE を許す（列スコープ GRANT）。quotas / status 等の他列は touch できない。
-- tenants に RLS は無いが、更新は set_require_signed_components が WHERE id = 自テナント に限定する。
GRANT UPDATE (require_signed_components) ON tenants TO faas_app;
