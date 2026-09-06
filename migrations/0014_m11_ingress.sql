-- M11 (§4.2): 公開 HTTP ingress gateway の opt-in フラグ。
--
-- `<app>.<tenant>.<base>` の公開 URL から到達できるのは、このフラグを **明示的に**
-- 立てた component だけ（既定 false = deny-by-default。プラットフォームの二重
-- deny-by-default 姿勢と一致）。egress（wasi:http/outgoing-handler / M9c allowlist）
-- とは無関係で、これは **ingress** のみを制御する。
--
-- GRANT は不要: 0004_rls.sql が faas_app に `UPDATE ON components` を列挙済みで、
-- components は tenant_isolation の FORCE RLS 下にある（列追加は RLS を変えない）。
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS ingress_enabled boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN components.ingress_enabled IS
    'M11: 公開 HTTP ingress gateway から到達可能か（deny-by-default, §4.2）。';
