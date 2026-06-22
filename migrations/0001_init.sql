-- M1: Walking Skeleton スキーマ（仕様書 §3.2 を M1 範囲に縮約）
--
-- 原則(§15): M3 でのテナント分離・RLS 一括導入(全レイヤ改修)に備え、
-- 全テーブルに tenant_id 列を「列だけ」用意しておく。M1 は RLS 無し・単一テナント 'default'。
--
-- TODO(§3.2): M3 で ENABLE/FORCE ROW LEVEL SECURITY と RLS ポリシー、
--             BYPASSRLS を持たない非特権ロール、SET LOCAL app.tenant_id を導入する。
-- TODO(§3.2): M3 で audit_logs / users / api_tokens / tenants.quotas を追加する。
-- 状態 enum は TEXT + CHECK 制約で表現する（M1 範囲）。

CREATE TABLE tenants (
    id          TEXT PRIMARY KEY,                 -- 例: ten_xxx / M1 は 'default'
    slug        TEXT NOT NULL UNIQUE,             -- 人間可読・グローバル一意
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active'    -- active | suspended
                CHECK (status IN ('active', 'suspended')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE components (
    id                TEXT PRIMARY KEY,           -- 例: cmp_xxx
    tenant_id         TEXT NOT NULL REFERENCES tenants(id),
    name              TEXT NOT NULL,              -- テナント内一意
    active_version_id TEXT,                       -- latest 解決 / ロールバック用（§6.7）
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at        TIMESTAMPTZ,                -- soft delete
    UNIQUE (tenant_id, name)
);

CREATE TABLE component_versions (
    id              TEXT PRIMARY KEY,             -- 例: ver_xxx
    component_id    TEXT NOT NULL REFERENCES components(id),
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    version         TEXT NOT NULL,                -- semver（不変）
    storage_uri     TEXT NOT NULL,                -- Object Storage 上の本体 URI（M1 は COMPONENTS_DIR 参照のプレースホルダ可）
    wasm_sha256     TEXT NOT NULL,                -- 整合性・キャッシュキー
    resource_limits JSONB NOT NULL DEFAULT '{}',  -- §4.3 / faas_shared::ResourceLimits を格納
    status          TEXT NOT NULL DEFAULT 'pending' -- pending | active | deprecated
                    CHECK (status IN ('pending', 'active', 'deprecated')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (component_id, version)
);

CREATE TABLE executions (
    id           TEXT PRIMARY KEY,                -- 例: exec_xxx（サーバ生成）
    tenant_id    TEXT NOT NULL REFERENCES tenants(id),
    component_id TEXT NOT NULL REFERENCES components(id),
    version_id   TEXT NOT NULL REFERENCES component_versions(id),
    status       TEXT NOT NULL DEFAULT 'pending'  -- pending | running | succeeded | failed | timeout
                 CHECK (status IN ('pending', 'running', 'succeeded', 'failed', 'timeout')),
    input        JSONB,                           -- M1: インライン入力（§6.4 大入力退避は M2 以降）
    output       JSONB,                           -- M1: インライン出力
    error        JSONB,                           -- 分類・メッセージ（§6.5）
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ
);

CREATE INDEX idx_executions_tenant_created ON executions (tenant_id, created_at DESC);
CREATE INDEX idx_executions_component_status ON executions (component_id, status);

-- M1: 単一テナント 'default' を 1 行 seed。
INSERT INTO tenants (id, slug, name, status)
VALUES ('default', 'default', 'Default Tenant', 'active');
