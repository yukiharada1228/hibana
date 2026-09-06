-- M17: Durable Object alarms（Cloudflare Workers DO Alarms 互換）。
--
-- DO インスタンス (tenant, class, id) ごとに **1 つの one-shot アラーム** を持つ（Workers 同様、
-- setAlarm は上書き）。fire は cron と同じく「専用ストア → スケジューラが due を単一発火 → 既存の
-- enqueue 正規パスで invoke」。メッセージ本体テーブルは持たず、consumer コンポーネントの invoke
-- （POST /__hibana/alarm）として JetStream パイプラインに載せる（再試行/backoff/DLQ/計量を再利用）。
--
-- alarm() が fire されると行は削除される（one-shot）。alarm() 内で再度 setAlarm すれば周期化できる。
CREATE TABLE IF NOT EXISTS do_alarms (
    tenant_id    TEXT        NOT NULL REFERENCES tenants(id),
    component_id TEXT        NOT NULL,
    do_class     TEXT        NOT NULL,
    do_id        TEXT        NOT NULL,
    scheduled_at TIMESTAMPTZ NOT NULL,                 -- 常に UTC。この時刻以降に fire する
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, do_class, do_id),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)
);

ALTER TABLE do_alarms ENABLE ROW LEVEL SECURITY;
ALTER TABLE do_alarms FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON do_alarms
    USING      (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

GRANT SELECT, INSERT, UPDATE, DELETE ON do_alarms TO faas_app;

-- スケジューラの due スキャン用（scheduled_at 昇順）。
CREATE INDEX IF NOT EXISTS idx_do_alarms_due ON do_alarms (scheduled_at);

-- 全テナント巡回用 SECURITY DEFINER 関数（cron_due_tenant_jobs と同型）。スケジューラは GUC 無しで
-- due な (tenant_id, do_class, do_id) だけを引き、各テナントごとに set_tenant_guc した tx で
-- 単一発火（FOR UPDATE SKIP LOCKED → delete → enqueue）を RLS 下で行う。
CREATE OR REPLACE FUNCTION do_due_alarms()
    RETURNS TABLE(tenant_id TEXT, do_class TEXT, do_id TEXT)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT tenant_id, do_class, do_id
    FROM do_alarms
    WHERE scheduled_at <= now()
    ORDER BY scheduled_at
    LIMIT 500;
$$;
REVOKE EXECUTE ON FUNCTION do_due_alarms() FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION do_due_alarms() TO   faas_app;
