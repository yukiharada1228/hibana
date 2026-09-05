-- M15: Queues バインディング（Cloudflare Workers Queues 互換）。
--
-- メッセージ本体は専用テーブルを持たず、**consumer コンポーネントの invoke** として既存の
-- JetStream パイプラインに載せる（再試行/backoff/DLQ/計量をそのまま再利用）。ここで持つのは
-- 「どの queue を どの component が consume するか」の登録だけ（tenant 境界は RLS）。
CREATE TABLE IF NOT EXISTS queue_consumers (
    tenant_id    TEXT        NOT NULL REFERENCES tenants(id),
    queue        TEXT        NOT NULL,
    component_id TEXT        NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, queue),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)
);

ALTER TABLE queue_consumers ENABLE ROW LEVEL SECURITY;
ALTER TABLE queue_consumers FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON queue_consumers
    USING      (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

GRANT SELECT, INSERT, UPDATE, DELETE ON queue_consumers TO faas_app;
