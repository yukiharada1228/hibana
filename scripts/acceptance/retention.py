"""Expired synthetic history under real HTTP load, only in the owned test cluster."""
import json
import time


class RetentionProbe:
    def __init__(self, operations, folder):
        self.operations, self.folder = operations, folder
        self.seeded = self.cycles = self.max_pending = 0
        self.started = self.last_seed = self.pending_since = None
        self.max_drain_seconds = 0

    def observe(self):
        now = time.monotonic()
        if self.started is None:
            if not (self.folder / 'load-start.json').exists():
                return None
            config = json.loads(self.operations.call('get', 'configmap', 'hibana-config', '-o', 'json'))['data']
            if config.get('EXECUTION_RETENTION_DAYS') != '30':
                raise ValueError('Acceptance requires the default 30-day history retention')
            self.operations.sql("""
                INSERT INTO tenants(id,slug,name,status) VALUES ('retention-fixture','retention-fixture','Retention fixture','suspended');
                INSERT INTO components(id,tenant_id,name) VALUES ('retention-app','retention-fixture','retention-app');
                INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256)
                    VALUES ('retention-v1','retention-fixture','retention-app','1','unused-fixture','abcd');
                INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,finished_at)
                    VALUES ('retention-keep','retention-fixture','retention-app','retention-v1','succeeded',true,now());
                INSERT INTO usage_rollups(tenant_id,component_id,period_start,invocation_count)
                    VALUES ('retention-fixture','retention-app',current_date-40,0);
                INSERT INTO audit_logs(tenant_id,actor,action,target,created_at)
                    VALUES ('retention-fixture','fixture','retention.fixture','retention-app',now()-interval '40 days');
            """)
            self.started = now
        row = json.loads(self.operations.sql("""
            SELECT json_build_object(
              'pending',count(*) FILTER (WHERE id LIKE 'retention-expired-%'),
              'protected',count(*) FILTER (WHERE id='retention-keep'),
              'usage',(SELECT invocation_count FROM usage_rollups WHERE tenant_id='retention-fixture'),
              'audit',(SELECT count(*) FROM audit_logs WHERE tenant_id='retention-fixture')
            ) FROM executions WHERE tenant_id='retention-fixture'
        """))
        if row['protected'] != 1 or row['audit'] != 1 or row['usage'] != self.seeded:
            raise ValueError('History cleanup changed retained history, usage or audit evidence')
        self.max_pending = max(self.max_pending, row['pending'])
        if row['pending'] and self.pending_since is not None and now-self.pending_since > 150:
            raise ValueError('Expired history did not drain within 150 seconds')
        if not row['pending'] and self.pending_since is not None:
            self.max_drain_seconds = max(self.max_drain_seconds, now-self.pending_since)
            self.pending_since = None
        loading = not (self.folder / 'application.json').exists()
        if loading and not row['pending'] and (self.last_seed is None or now-self.last_seed >= 300):
            # Integers are generated locally; no user SQL or live tenant is used.
            self.cycles += 1
            self.operations.sql(f"""
                BEGIN;
                INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at,application_logs)
                    SELECT 'retention-expired-{self.cycles}-'||i,'retention-fixture','retention-app','retention-v1',
                      (ARRAY['succeeded','failed','timeout'])[1+i%3],true,
                      now()-interval '41 days',now()-interval '40 days',
                      '{{"stdout":"expired fixture","stderr":"","truncated":false}}'::jsonb
                    FROM generate_series(1,6000) AS i;
                UPDATE usage_rollups SET invocation_count=invocation_count+6000 WHERE tenant_id='retention-fixture';
                COMMIT;
            """)
            self.seeded += 6000
            self.max_pending = max(self.max_pending, 6000)
            self.last_seed = self.pending_since = now
            row['pending'] = 6000
        row.update(seeded=self.seeded, cycles=self.cycles, deleted=self.seeded-row['pending'])
        return row

    def finish(self):
        deadline = time.monotonic()+160
        while True:
            row = self.observe()
            if row and self.seeded and not row['pending']:
                break
            if time.monotonic()>deadline:
                raise ValueError('Retention evidence was incomplete')
            time.sleep(2)
        result = dict(row, passed=True, max_observed_pending=self.max_pending,
                      max_observed_drain_seconds=self.max_drain_seconds)
        (self.folder / 'retention.json').write_text(json.dumps(result, indent=2)+'\n')
        return result
