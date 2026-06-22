-- M3b: テナント分離を「WHERE 句」から「多層防御の境界」へ昇格させる（仕様書 §3.2 / §15）。
--
-- 0001-0003 への ADD のみ（additive）。sqlx migrate ランナー（control-plane 起動時）が
-- 0003 の後に適用する（BASELINE_VERSIONS には入れない＝MIGRATOR.run が通常適用する）。
-- このマイグレーションは**テーブル所有ロール**（0001-0003 を適用したロール）で実行する
-- 必要がある: CREATE ROLE / ALTER TABLE ... FORCE RLS / CREATE FUNCTION SECURITY DEFINER は
-- 所有権・CREATEROLE を要する（ランタイムの faas_app では実行できない）。
--
-- 設計（USER-CONFIRMED M3b DECISIONS）:
--  1. 認証前（テナントコンテキスト確立前）の 3 つの参照 = SECURITY DEFINER 関数で包む。
--     BYPASSRLS ロールは使わない。それ以外は FORCE RLS の下に置く。
--  3. GUC は FAIL-CLOSED: current_setting('app.tenant_id') を第 2 引数なしで使い、未設定なら
--     ERROR（SET LOCAL 忘れは漏洩せず必ず失敗する）。GUC は set_config(...,true) で設定する。

-- 1. 非特権アプリロール（BYPASSRLS なし・SUPERUSER なし）。
--    control-plane / worker の DATABASE_URL がこのロールで接続する。
--    パスワードは実プロビジョニングで env から流す（プレースホルダ）。
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'faas_app') THEN
        CREATE ROLE faas_app LOGIN PASSWORD 'faas_app' NOBYPASSRLS;
    END IF;
END $$;

-- 2. テナント 5 テーブルで RLS を ENABLE + FORCE（所有者にも適用させる）。
--    tenants は tenant_id 列を持たないため対象外（slug 参照は SECURITY DEFINER 関数で行う）。
ALTER TABLE components         ENABLE ROW LEVEL SECURITY; ALTER TABLE components         FORCE ROW LEVEL SECURITY;
ALTER TABLE component_versions ENABLE ROW LEVEL SECURITY; ALTER TABLE component_versions FORCE ROW LEVEL SECURITY;
ALTER TABLE executions         ENABLE ROW LEVEL SECURITY; ALTER TABLE executions         FORCE ROW LEVEL SECURITY;
ALTER TABLE users              ENABLE ROW LEVEL SECURITY; ALTER TABLE users              FORCE ROW LEVEL SECURITY;
ALTER TABLE api_tokens         ENABLE ROW LEVEL SECURITY; ALTER TABLE api_tokens         FORCE ROW LEVEL SECURITY;

-- 3. fail-closed ポリシー（第 2 引数フォールバックなし → 未設定 GUC は ERROR）。
--    current_setting は text を返し、tenant_id 列は TEXT なのでキャスト不要。USING + WITH CHECK 両方。
CREATE POLICY tenant_isolation ON components
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON component_versions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON executions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON users
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON api_tokens
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- 4. faas_app への権限付与（FORCE RLS は引き続き適用される。GRANT は base 権限のみ）。
GRANT USAGE ON SCHEMA public TO faas_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON components, component_versions, executions, users, api_tokens TO faas_app;
GRANT SELECT ON tenants TO faas_app;   -- users の FK・slug definer 関数の所有者経由参照に必要
GRANT INSERT ON tenants TO faas_app;   -- bootstrap_tenant は tenants を INSERT（tenants に RLS なし）
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO faas_app;  -- 将来用（audit_logs BIGSERIAL は M3c）

-- 5. 認証前参照の 3 関数（SECURITY DEFINER）。テーブル所有者が所有し（definer != faas_app）、
--    EXECUTE は faas_app に付与する。所有者として実行されるため faas_app の FORCE RLS の対象外で、
--    GUC 未設定のまま認証前参照が成功する。各関数は STABLE・必要列のみ返却・パラメータ化。

-- 5a. 認証ホットパス: token_hash 照合（テナントを「確立する」参照）。
CREATE FUNCTION auth_lookup_token_by_hash(p_hash text)
    RETURNS TABLE (
        id          text,
        tenant_id   text,
        user_id     text,
        scopes      text[],
        expires_at  timestamptz,
        revoked_at  timestamptz,
        user_role   text
    )
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT t.id, t.tenant_id, t.user_id, t.scopes, t.expires_at, t.revoked_at, u.role
        FROM api_tokens t
        LEFT JOIN users u ON u.id = t.user_id AND u.deleted_at IS NULL
        WHERE t.token_hash = p_hash
    $$;

-- 5b. login: (tenant_id, email) でユーザ解決。
CREATE FUNCTION auth_lookup_user_by_email(p_tenant text, p_email text)
    RETURNS TABLE (
        id            text,
        password_hash text,
        role          text
    )
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT id, password_hash, role FROM users
        WHERE tenant_id = p_tenant AND email = p_email AND deleted_at IS NULL
    $$;

-- 5c. login: slug -> tenant_id（active のみ）。NULL スカラの誤判定を避けるため
--     RETURNS TABLE(id text) にする（呼び出し側は fetch_optional で None を得る）。
CREATE FUNCTION auth_lookup_tenant_id_by_slug(p_slug text)
    RETURNS TABLE (id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT id FROM tenants WHERE slug = p_slug AND status = 'active'
    $$;

-- 既定の PUBLIC 実行権を剥奪し、faas_app にのみ EXECUTE を付与する。
REVOKE EXECUTE ON FUNCTION auth_lookup_token_by_hash(text)         FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION auth_lookup_user_by_email(text, text)   FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION auth_lookup_tenant_id_by_slug(text)     FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION auth_lookup_token_by_hash(text)         TO faas_app;
GRANT  EXECUTE ON FUNCTION auth_lookup_user_by_email(text, text)   TO faas_app;
GRANT  EXECUTE ON FUNCTION auth_lookup_tenant_id_by_slug(text)     TO faas_app;
