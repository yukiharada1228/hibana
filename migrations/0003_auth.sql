-- M3a: 認証・認可スキーマ（仕様書 §3.2 / §3.3 / §6.0）。
--
-- 0001 (M1) / 0002 (M2) への ADD のみ。TEXT id、TEXT + CHECK enum、additive の規約に従う。
-- 原則(§15): RLS / テナント分離は後続スライスで一括導入する。M3a はスキーマ + 認証契約のみ。
--
-- 注意: これ以降のマイグレーションは sqlx migrate ランナー（control-plane 起動時）で
--       適用される。0001/0002 は手動適用済みのため、ランナーは _sqlx_migrations の
--       baseline 行で再適用をスキップする（main.rs の reconcile_baseline を参照）。

-- §3.3: テナント内ユーザ。email はテナント内一意。password_hash は argon2id。
-- role は付与可能スコープの上限（faas_shared::Role::ceiling）を定める。
CREATE TABLE users (
    id            TEXT PRIMARY KEY,                          -- 例: usr_xxx
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    email         TEXT NOT NULL,
    password_hash TEXT NOT NULL,                             -- argon2id（pure-Rust）
    role          TEXT NOT NULL                              -- member | admin
                  CHECK (role IN ('member', 'admin')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at    TIMESTAMPTZ,                               -- soft delete
    UNIQUE (tenant_id, email)
);

-- §3.3: API トークン。opaque secret は一度だけ返却し、DB には sha256(secret) hex を保存する。
-- user_id は NULL 可（テナント直属のサービストークン）。scopes は read/invoke/deploy/admin の部分集合。
CREATE TABLE api_tokens (
    id          TEXT PRIMARY KEY,                            -- 例: tok_xxx
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    user_id     TEXT REFERENCES users(id),                   -- NULL 可（サービストークン）
    token_hash  TEXT NOT NULL UNIQUE,                        -- sha256(secret) hex。提示トークンをハッシュして照合
    scopes      TEXT[] NOT NULL
                CHECK (scopes <@ ARRAY['read', 'invoke', 'deploy', 'admin']),
    name        TEXT,                                        -- 人間可読ラベル（任意）
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,                        -- 失効時刻（必須）
    revoked_at  TIMESTAMPTZ                                  -- 明示失効（DELETE /tokens/{id}）
);

-- 認証ホットパス（token_hash 照合）後のテナント絞り込み・一覧用。
CREATE INDEX idx_api_tokens_tenant ON api_tokens (tenant_id);
