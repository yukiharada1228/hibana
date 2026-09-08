#!/usr/bin/env python3
"""Backup, restore and fault drills for this checkout's kind cluster only."""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import hmac
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
from urllib.parse import quote, urlsplit
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import xml.etree.ElementTree as ET

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sdk/platform"))
from kubernetes import LocalCluster, ROOT
from bounded_process import run as run_bounded


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


class Operations:
    def __init__(self, cluster):
        self.cluster = cluster
        cluster.require_owned()
        self.base = [*cluster.kubectl, '-n', 'hibana']
        # A kubeconfig pathname alone is insufficient proof of ownership.
        server = json.loads(self.call('config', 'view', '--minify', '-o', 'json'))['clusters'][0]['cluster']['server']
        if urlsplit(server).hostname not in ('127.0.0.1', 'localhost'):
            raise ValueError('Only the checkout-owned local kind endpoint is supported.')
        nodes = json.loads(self.call('get', 'nodes', '-o', 'json'))['items']
        if not nodes or any(not n['metadata']['name'].startswith(cluster.name + '-') for n in nodes):
            raise ValueError('Unexpected nodes; refusing to operate on this cluster.')

    def call(self, *args, input=None, stdin=None, output=None, timeout=None):
        budget = timeout if timeout is not None else (260 if args[0] in ('rollout', 'wait') else 30)
        return run_bounded([*self.base, *map(str, args)], input=input, stdin=stdin,
                           stdout=output if output is not None else subprocess.PIPE, timeout=budget)

    def deployment(self, name):
        return json.loads(self.call('get', f'deployment/hibana-{name}', '-o', 'json'))

    def scale(self, name, replicas):
        self.call('scale', f'deployment/hibana-{name}', f'--replicas={replicas}')
        if replicas:
            self.call('rollout', 'status', f'deployment/hibana-{name}', '--timeout=240s')
        else:
            self.call('wait', '--for=delete', 'pod', '-l', f'app.kubernetes.io/name=hibana-{name}', '--timeout=240s')

    def close_admission(self, owner):
        claimed = self.sql("UPDATE platform_maintenance SET owner='" + owner +
                           "' WHERE singleton AND owner IS NULL RETURNING owner")
        if claimed != owner:
            raise ValueError('Maintenance is already owned by another operation; no replicas changed.')

    def open_admission(self, owner):
        changed = self.sql("UPDATE platform_maintenance SET owner=NULL WHERE singleton AND owner='" + owner + "' RETURNING singleton")
        if changed != 't':
            raise ValueError('Maintenance ownership changed; admission was not reopened.')

    def ready_workers(self, replicas, timeout=30):
        pods = json.loads(self.call('get', 'pods', '-l', 'app.kubernetes.io/name=hibana-worker',
                                    '-o', 'json', timeout=timeout))['items']
        result = {}
        for pod in pods:
            status = pod.get('status', {})
            if (pod['metadata'].get('deletionTimestamp') or not status.get('podIP') or
                    not any(c['type'] == 'Ready' and c['status'] == 'True' for c in status.get('conditions', []))):
                raise ValueError('Worker fleet is not ready; admission remains closed.')
            result[pod['metadata']['uid']] = (status['podIP'], sum(
                c.get('restartCount', 0) for c in status.get('containerStatuses', [])))
        if len(result) != replicas:
            raise ValueError('Worker replica count changed; admission remains closed.')
        return result

    def prepare_apps(self, owner, replicas, timeout=300):
        # Pod readiness only proves the process is listening. Compile all active
        # artifacts while admission is still closed, then verify the same ready Pods.
        _, values = self.credentials()
        deadline = time.monotonic() + timeout
        def remaining(limit):
            seconds = deadline - time.monotonic()
            if seconds <= 0:
                raise ValueError('Application preparation timed out; admission remains closed.')
            return min(limit, seconds)
        with self.forward('control-plane', 8081, startup_timeout=remaining(15)) as endpoint:
            while True:
                before = self.ready_workers(replicas, timeout=remaining(30))
                request = Request(endpoint + '/internal/maintenance/prepare', method='POST',
                                  headers={'Authorization': 'Bearer ' + values['BOOTSTRAP_ADMIN_TOKEN'],
                                           'Content-Type': 'application/json'},
                                  data=json.dumps({'owner': owner, 'workers': sorted(v[0] for v in before.values())}).encode())
                try:
                    with urlopen(request, timeout=remaining(245)) as response:
                        if response.status != 204:
                            raise ValueError('Unexpected preparation response; admission remains closed.')
                except HTTPError as error:
                    code = error.code
                    error.close()
                    if code != 503:
                        raise ValueError(f'Application preparation rejected ({code}); admission remains closed.') from None
                    # DNS can still contain pre-maintenance Pod IPs. Retry only the
                    # idempotent preparation request, bounded by the overall deadline.
                    time.sleep(remaining(1))
                    continue
                except (URLError, TimeoutError):
                    raise ValueError('Application preparation transport failed; admission remains closed.') from None
                after = self.ready_workers(replicas, timeout=remaining(30))
                if before == after:
                    return
                time.sleep(remaining(1))

    def drain(self, timeout=180):
        # The durable DB gate also covers replacement Pods. Check each current CP
        # for requests that passed the gate before it closed, including slow uploads.
        from contextlib import ExitStack
        _, values = self.credentials()
        deadline = time.monotonic() + timeout
        def remaining(limit):
            seconds = deadline - time.monotonic()
            if seconds <= 0:
                raise ValueError('Maintenance drain timed out; backup not started.')
            return min(limit, seconds)
        with ExitStack() as stack:
            endpoints = {}
            while time.monotonic() < deadline:
                pods = json.loads(self.call('get', 'pods', '-l', 'app.kubernetes.io/name=hibana-control-plane', '-o', 'json', timeout=remaining(30)))['items']
                active = 0
                for pod in pods:
                    uid, name = pod['metadata']['uid'], pod['metadata']['name']
                    if uid not in endpoints:
                        endpoints[uid] = stack.enter_context(self.forward('control-plane', 8081, resource=f'pod/{name}', startup_timeout=remaining(15)))
                    request = Request(endpoints[uid] + '/internal/maintenance',
                                      headers={'Authorization': 'Bearer ' + values['BOOTSTRAP_ADMIN_TOKEN']})
                    with urlopen(request, timeout=remaining(5)) as response:
                        active += json.load(response)['active_requests']
                current = json.loads(self.call('get', 'pods', '-l', 'app.kubernetes.io/name=hibana-control-plane', '-o', 'json', timeout=remaining(30)))['items']
                stable = {p['metadata']['uid'] for p in current} == {p['metadata']['uid'] for p in pods}
                executions = int(self.sql("SELECT count(*) FROM executions WHERE status IN ('pending','running')", timeout=remaining(30)))
                if stable and active == 0 and executions == 0:
                    return
                time.sleep(0.5)
        raise ValueError('Maintenance drain timed out; backup not started.')

    @contextmanager
    def stopped_apps(self):
        hpas = json.loads(self.call('get', 'hpa', '-o', 'json'))['items']
        if hpas:
            raise ValueError('Remove/pause HPA before a maintenance window; no changes made.')
        counts = {n: self.deployment(n)['spec']['replicas'] for n in ('control-plane', 'worker')}
        owner = os.urandom(16).hex()
        self.close_admission(owner)
        try:
            self.drain()
            self.scale('worker', 0)
            self.scale('control-plane', 0)
            yield counts
        finally:
            errors = []
            # Completion/secret APIs must exist before workers start again.
            for name in ('control-plane', 'worker'):
                try:
                    self.scale(name, counts[name])
                except Exception as error:
                    errors.append(str(error))
            if errors:
                raise ValueError('Admission remains closed. Restore replicas manually: ' + '; '.join(errors))
            if any(counts.values()):
                if not all(counts.values()):
                    raise ValueError('Both control-plane and Worker replicas are required; admission remains closed.')
                self.prepare_apps(owner, counts['worker'])
                self.open_admission(owner)

    @contextmanager
    def forward(self, name, port, resource=None, startup_timeout=15):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            local_port = sock.getsockname()[1]
        child = subprocess.Popen([*self.base, 'port-forward', '--address', '127.0.0.1',
                                  resource or f'deployment/hibana-{name}', f'{local_port}:{port}'],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + startup_timeout
            while time.monotonic() < deadline:
                if child.poll() is not None:
                    raise ValueError('Port-forward failed')
                try:
                    with socket.create_connection(('127.0.0.1', local_port), timeout=0.2):
                        break
                except OSError:
                    time.sleep(0.1)
            else:
                raise ValueError('Port-forward startup timed out')
            yield f'http://127.0.0.1:{local_port}'
        finally:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)

    def sql(self, query, database='hibana', timeout=30):
        return self.call('exec', 'deployment/hibana-postgres', '--', 'psql', '-X', '-q', '-U', 'hibana_admin',
                         '-d', database, '-v', 'ON_ERROR_STOP=1', '-Atc', query, timeout=timeout).decode().strip()

    def credentials(self):
        import base64
        names = ['hibana-runtime', 'hibana-control-plane', 'hibana-migration', 'hibana-local-dependencies']
        result = {'apiVersion': 'v1', 'kind': 'List', 'items': []}
        for name in names:
            item = json.loads(self.call('get', 'secret', name, '-o', 'json'))
            result['items'].append({'apiVersion': 'v1', 'kind': 'Secret', 'type': 'Opaque',
                                    'metadata': {'name': name, 'namespace': 'hibana'}, 'data': item['data']})
        values = {k: base64.b64decode(v).decode() for item in result['items'][:2] for k, v in item['data'].items()}
        return result, values

    def snapshot(self, folder):
        folder.mkdir(parents=True, mode=0o700)  # refuse to overwrite any backup
        folder.chmod(0o700)
        credentials, values = self.credentials()
        (folder / 'secrets.json').write_text(json.dumps(credentials))
        with (folder / 'postgres.dump').open('xb') as stream:
            self.call('exec', 'deployment/hibana-postgres', '--', 'pg_dump', '-U', 'hibana_admin',
                      '-d', 'hibana', '-Fc', output=stream, timeout=600)
        config = json.loads(self.call('get', 'configmap', 'hibana-config', '-o', 'json'))['data']
        key_config = self.key_config(config, values)
        (folder / 'key-config.json').write_text(json.dumps(key_config))
        bucket = config['S3_BUCKET']
        files = {}
        with self.forward('minio', 9000) as endpoint:
            s3 = S3(endpoint, values['S3_ACCESS_KEY'], values['S3_SECRET_KEY'], config.get('S3_REGION', 'us-east-1'), bucket)
            for index, key in enumerate(s3.keys()):
                filename = f'object-{index:08d}'
                with s3.request('GET', key) as response, (folder / filename).open('xb') as stream:
                    while chunk := response.read(1024 * 1024):
                        stream.write(chunk)
                files[key] = {'file': filename, 'sha256': sha(folder / filename)}
        manifest = {'format': 2, 'created_at': datetime.now(timezone.utc).isoformat(),
                    'key_config_sha256': sha(folder / 'key-config.json'),
                    'cluster': self.cluster.name, 'bucket': bucket, 'objects': files,
                    'postgres_sha256': sha(folder / 'postgres.dump'), 'secrets_sha256': sha(folder / 'secrets.json'),
                    'counts': {table: int(self.sql(f'SELECT count(*) FROM {table}'))
                               for table in ('tenants', 'components', 'component_versions', 'function_secrets', 'function_secret_versions', 'executions', 'version_configs', 'version_secret_bindings')}}
        # Written last; interrupted snapshots have no complete manifest.
        (folder / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        verify_backup(folder)
        return manifest

    def key_config(self, config, values):
        # Keep key identities, without copying source-cluster network endpoints.
        result = {}
        container = self.deployment('control-plane')['spec']['template']['spec']['containers'][0]
        expected = [{'configMapRef': {'name': 'hibana-config'}},
                    {'secretRef': {'name': 'hibana-runtime'}}, {'secretRef': {'name': 'hibana-control-plane'}}]
        if container.get('envFrom') != expected:
            raise ValueError('Local backup requires the standard CP envFrom sources and order; custom sources need an explicit backup mapping.')
        for entry in container.get('env', []):
            if entry['name'] in ('SECRETS_MASTER_KEY', 'SECRETS_RETIRED_KEYS', 'JOB_SIGNING_KEY'):
                if entry.get('value') != values.get(entry['name'], ''):
                    raise ValueError('Runtime key material override differs from backed-up Secrets.')
        for key in ('SECRETS_MASTER_KID', 'JOB_SIGNING_KID'):
            value = values.get(key, config.get(key))
            for entry in container.get('env', []):
                if entry['name'] == key:
                    if 'value' not in entry:
                        raise ValueError('Key identity overrides must use explicit values for local backup.')
                    value = entry['value']
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f'Missing runtime key identity: {key}')
            result[key] = value
        return result

    def verify_secrets(self, folder, manifest, database):
        import base64
        if manifest['format'] != 2:
            raise ValueError('Legacy backup has no key identities; supply an audited key-config.json and format-2 manifest before restoring.')
        config = json.loads((folder / 'key-config.json').read_text())
        credentials = json.loads((folder / 'secrets.json').read_text())
        values = {}
        for name in ('hibana-runtime', 'hibana-control-plane'):
            item = next(s for s in credentials['items'] if s['metadata']['name'] == name)
            values.update({k: base64.b64decode(v).decode() for k, v in item['data'].items()})
        keys = {'active_kid': config['SECRETS_MASTER_KID'], 'active_key': values['SECRETS_MASTER_KEY'],
                'retired': values.get('SECRETS_RETIRED_KEYS', '')}
        # Current live generations are exactly what a new execution can redeem
        # after a drained restore. Historical retired generations may intentionally
        # have had their KEKs removed after rekey; they are still retained in the dump.
        rows = self.sql("""SELECT json_build_object('tenant_id',s.tenant_id,'component_id',s.component_id,
            'secret_id',s.id,'name',s.name,'version',v.version,'kek_kid',v.kek_kid,
            'ciphertext',encode(v.ciphertext,'hex'),'nonce',encode(v.nonce,'hex'),
            'wrapped_dek',encode(v.wrapped_dek,'hex'),'dek_nonce',encode(v.dek_nonce,'hex'),'value_len',v.value_len)
            FROM function_secrets s JOIN function_secret_versions v
              ON v.tenant_id=s.tenant_id AND v.secret_id=s.id AND v.version=s.current_version
            WHERE s.deleted_at IS NULL ORDER BY s.id""", database)
        expected = int(self.sql('SELECT count(*) FROM function_secrets WHERE deleted_at IS NULL', database))
        image = self.deployment('control-plane')['spec']['template']['spec']['containers'][0]['image']
        name = 'hibana-backup-verifier-' + os.urandom(8).hex()
        try:
            count = run_bounded(['docker', 'run', '-i', '--name', name, '--network', 'none',
                                 '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                                 '--memory', '128m', '--pids-limit', '64', '--entrypoint',
                                 '/usr/local/bin/hibana-control-plane', image, '--verify-backup-secrets'],
                                input=(json.dumps(keys) + '\n' + (rows + '\n' if rows else '')).encode(), timeout=120)
            if int(count) != expected:
                raise ValueError('Restored secret count mismatch')
        finally:
            # Docker daemon owns containers; killing its CLI alone is insufficient.
            run_bounded(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL)

    def verify_restore(self, folder, manifest):
        # Restore the real custom dump into an isolated temporary database, not over live data.
        database = 'hibana_restore_' + os.urandom(6).hex()
        self.sql(f'CREATE DATABASE {database}', 'postgres')
        try:
            with (folder / 'postgres.dump').open('rb') as stream:
                self.call('exec', '-i', 'deployment/hibana-postgres', '--', 'pg_restore', '-U', 'hibana_admin',
                          '-d', database, '--exit-on-error', '--no-owner', stdin=stream, timeout=600)
            for table, count in manifest['counts'].items():
                if int(self.sql(f'SELECT count(*) FROM {table}', database)) != count:
                    raise ValueError(f'Restored row count mismatch: {table}')
            self.verify_secrets(folder, manifest, database)
        finally:
            self.sql(f'DROP DATABASE {database} WITH (FORCE)', 'postgres')

    def backup(self, folder):
        with self.stopped_apps():
            manifest = self.snapshot(folder)
            self.verify_restore(folder, manifest)
        print(f'Backup and database restore verification passed: {folder} ({len(manifest["objects"])} objects)')

    def restore_empty(self, folder):
        manifest = verify_backup(folder)
        if int(self.sql("SELECT count(*) FROM pg_tables WHERE schemaname='public'")):
            raise ValueError('Restore target database is not empty; refusing to overwrite it.')
        credentials = json.loads((folder / 'secrets.json').read_text())
        # Same credential set is required so existing encrypted secrets remain decryptable.
        current, values = self.credentials()
        if current != credentials:
            raise ValueError('Restore target secrets differ; provision the backup secrets on the new target first.')
        config = json.loads(self.call('get', 'configmap', 'hibana-config', '-o', 'json'))['data']
        if manifest['format'] != 2 or self.key_config(config, values) != json.loads((folder / 'key-config.json').read_text()):
            raise ValueError('Restore target key identities differ; provision the backup key-config.json values first.')
        if config['S3_BUCKET'] != manifest['bucket']:
            raise ValueError('Restore bucket differs from backup')
        with self.forward('minio', 9000) as endpoint:
            s3 = S3(endpoint, values['S3_ACCESS_KEY'], values['S3_SECRET_KEY'], config.get('S3_REGION', 'us-east-1'), manifest['bucket'])
            s3.ensure_bucket()
            if list(s3.keys()):
                raise ValueError('Restore target bucket is not empty; refusing to overwrite it.')
            for key, item in manifest['objects'].items():
                payload = (folder / item['file']).read_bytes()
                with s3.request('PUT', key, data=payload):
                    pass
                with s3.request('GET', key) as response:
                    if hashlib.file_digest(response, 'sha256').hexdigest() != item['sha256']:
                        raise ValueError('Restored object checksum mismatch')
        with (folder / 'postgres.dump').open('rb') as stream:
            self.call('exec', '-i', 'deployment/hibana-postgres', '--', 'pg_restore', '-U', 'hibana_admin',
                      '-d', 'hibana', '--exit-on-error', '--no-owner', stdin=stream, timeout=600)
        for table, count in manifest['counts'].items():
            if int(self.sql(f'SELECT count(*) FROM {table}')) != count:
                raise ValueError(f'Restored row count mismatch: {table}')
        self.verify_secrets(folder, manifest, 'hibana')

    def persist(self, folder):
        for name in ('postgres', 'minio'):
            if any('persistentVolumeClaim' in v for v in self.deployment(name)['spec']['template']['spec']['volumes']):
                raise ValueError('Dependencies already have PVCs; use backup/restore for recovery.')
        with self.stopped_apps() as counts:
            manifest = self.snapshot(folder)
            self.verify_restore(folder, manifest)
            # Keep durable recovery instructions outside the ephemeral cluster.
            (folder / 'replicas.json').write_text(json.dumps(counts))
            try:
                self.call('apply', '-k', ROOT / 'deploy/kubernetes/persistent-dependencies')
                for name in ('postgres', 'minio'):
                    self.call('rollout', 'status', f'deployment/hibana-{name}', '--timeout=240s')
                self.restore_empty(folder)
            except BaseException:
                # A failed migration must not bring apps up against partial data.
                counts.update({name: 0 for name in counts})
                raise ValueError(f'Storage migration incomplete. Applications remain stopped. Backup: {folder}; original counts: replicas.json')
        print(f'Persistent storage migration and restore passed. Backup: {folder}')

    def fault(self, target, seconds, probe_host=None):
        from contextlib import ExitStack
        if probe_host is None:
            raise ValueError('Supply --probe-host for a read-only deployed HTTP fixture (for example hello.smoke.hibana.local).')
        with ExitStack() as stack:
            endpoint = stack.enter_context(self.forward('control-plane', 8083))
            def probe():
                try:
                    with urlopen(Request(endpoint + '/', headers={'Host': probe_host}), timeout=2) as response:
                        return response.status, response.read(65537)
                except Exception:
                    return 0, b''
            status, baseline = probe()
            if status != 200 or len(baseline) > 65536:
                raise ValueError('Probe fixture must return a small successful response before interruption.')
            return self.fault_with_probe(target, seconds, probe, baseline)

    def fault_with_probe(self, target, seconds, probe, baseline):
        observations = []
        if target in ('postgres', 'redis', 'minio'):
            # Pause the container instead of replacing Pods; emptyDir contents survive.
            pods = json.loads(self.call('get', 'pods', '-l', f'app.kubernetes.io/name=hibana-{target}', '-o', 'json'))['items']
            if len(pods) != 1:
                raise ValueError('This single-instance interruption drill requires exactly one dependency Pod.')
            pod = pods[0]
            node = pod['spec']['nodeName']
            container = pod['status']['containerStatuses'][0]['containerID'].removeprefix('containerd://')
            if not container.isalnum():
                raise ValueError('Unexpected container ID')
            # SIGSTOP keeps data but can trigger kubelet liveness restarts if probes change.
            start = ['docker', 'exec', node, 'ctr', '-n', 'k8s.io', 'tasks', 'pause', container]
            stop = ['docker', 'exec', node, 'ctr', '-n', 'k8s.io', 'tasks', 'resume', container]
        else:
            node = target
            if node not in (self.cluster.name + '-worker', self.cluster.name + '-worker2'):
                raise ValueError('Node drill accepts only an owned kind worker node, never control-plane.')
            info = json.loads(run_bounded(['docker', 'inspect', node]))[0]
            if info['Config']['Labels'].get('io.x-k8s.kind.cluster') != self.cluster.name or info['State']['Paused']:
                raise ValueError('Node is not an available owned kind container')
            start, stop = ['docker', 'pause', node], ['docker', 'unpause', node]
        began = time.monotonic()
        try:
            run_bounded(start, stdout=subprocess.DEVNULL)
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                observations.append({'seconds': round(time.monotonic() - began, 1),
                                     'pods': json.loads(self.call('get', 'pods', '-o', 'json', timeout=max(0.1, min(10, deadline - time.monotonic()))))['items'], 'http_status': probe()[0]})
                time.sleep(min(5, max(0, deadline - time.monotonic())))
        finally:
            run_bounded(stop, stdout=subprocess.DEVNULL)
        self.call('wait', '--for=condition=Ready', 'node', '--all', '--timeout=240s')
        for name in ('postgres', 'redis', 'minio', 'control-plane', 'worker'):
            self.call('rollout', 'status', f'deployment/hibana-{name}', '--timeout=240s')
        recovered = False
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            status, body = probe()
            if status == 200 and body == baseline:
                recovered = True
                break
            time.sleep(1)
        # Persist only operational status; Pod env/spec can contain credentials.
        result = {'target': target, 'http_recovered': recovered, 'interruption_seconds': seconds, 'recovered_seconds': round(time.monotonic() - began, 1),
                  'samples': [{'seconds': s['seconds'], 'http_status': s['http_status'], 'pods': [
                      {'name': p['metadata']['name'], 'phase': p['status']['phase'],
                       'ready': any(c['type'] == 'Ready' and c['status'] == 'True' for c in p['status'].get('conditions', [])),
                       'restarts': sum(c['restartCount'] for c in p['status'].get('containerStatuses', []))} for p in s['pods']]}
                              for s in observations]}
        output = self.cluster.state / f'fault-{target}-{int(time.time())}.json'
        output.write_text(json.dumps(result, indent=2) + '\n')
        if not recovered:
            raise ValueError(f'Infrastructure resumed but HTTP did not recover. Report: {output}')
        print(f'Interruption/recovery drill completed: {output}. HTTP recovery verified; this is not a primary failover test.')


class S3:
    """Minimal SigV4 client for local MinIO backup; credentials never enter argv."""
    def __init__(self, endpoint, key, secret, region, bucket):
        self.endpoint, self.key, self.secret, self.region, self.bucket = endpoint, key, secret, region, bucket

    def request(self, method, key=None, query=None, data=b''):
        now = datetime.now(timezone.utc)
        date, stamp = now.strftime('%Y%m%d'), now.strftime('%Y%m%dT%H%M%SZ')
        path = '/' + quote(self.bucket, safe='') + (('/' + quote(key, safe='/~')) if key is not None else '')
        query = '&'.join(f'{quote(k, safe="~")}={quote(str(v), safe="~")}' for k, v in sorted((query or {}).items()))
        digest = hashlib.sha256(data).hexdigest()
        host = urlsplit(self.endpoint).netloc
        headers = {'host': host, 'x-amz-content-sha256': digest, 'x-amz-date': stamp}
        signed = ';'.join(headers)
        canonical = '\n'.join([method, path, query, ''.join(f'{k}:{v}\n' for k, v in headers.items()), signed, digest])
        scope = f'{date}/{self.region}/s3/aws4_request'
        to_sign = f'AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}'
        signing = ('AWS4' + self.secret).encode()
        for part in (date, self.region, 's3', 'aws4_request'):
            signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
        signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
        headers['Authorization'] = f'AWS4-HMAC-SHA256 Credential={self.key}/{scope}, SignedHeaders={signed}, Signature={signature}'
        return urlopen(Request(self.endpoint + path + ('?' + query if query else ''),
                               data=data if method in ('PUT', 'POST') else None, headers=headers, method=method), timeout=30)

    def ensure_bucket(self):
        from urllib.error import HTTPError
        try:
            with self.request('HEAD'):
                return
        except HTTPError as error:
            if error.code != 404:
                raise
        with self.request('PUT'):
            pass

    def keys(self):
        token = None
        while True:
            query = {'list-type': '2'}
            if token:
                query['continuation-token'] = token
            with self.request('GET', query=query) as response:
                root = ET.parse(response).getroot()
            yield from (item.text for item in root.findall('{*}Contents/{*}Key'))
            token = root.findtext('{*}NextContinuationToken')
            if not token:
                return


def verify_backup(folder):
    manifest = json.loads((folder / 'manifest.json').read_text())
    if manifest['format'] not in (1, 2):
        raise ValueError('Unsupported backup format')
    entries = {'postgres.dump': manifest['postgres_sha256'], 'secrets.json': manifest['secrets_sha256']}
    if manifest['format'] == 2:
        entries['key-config.json'] = manifest['key_config_sha256']
    for item in manifest['objects'].values():
        filename = item['file']
        if not filename.startswith('object-') or not filename[7:].isdigit():
            raise ValueError('Invalid backup object filename')
        entries[filename] = item['sha256']
    for filename, expected in entries.items():
        path = folder / filename
        if path.is_symlink() or not path.is_file() or sha(path) != expected:
            raise ValueError(f'Backup checksum failed: {filename}')
    allowed_tables = {'tenants', 'components', 'component_versions', 'function_secrets', 'function_secret_versions', 'executions'}
    if set(manifest['counts']) not in (allowed_tables, allowed_tables | {'version_configs', 'version_secret_bindings'}):
        raise ValueError('Invalid backup table manifest')
    if manifest['format'] == 2:
        config = json.loads((folder / 'key-config.json').read_text())
        if set(config) != {'SECRETS_MASTER_KID', 'JOB_SIGNING_KID'} or any(not isinstance(v, str) or not v.strip() for v in config.values()):
            raise ValueError('Invalid backup key configuration')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['backup', 'verify', 'persist', 'restore', 'fault'])
    parser.add_argument('--cluster', default='hibana')
    parser.add_argument('--backup', type=Path)
    parser.add_argument('--probe-host', help='Host of a read-only HTTP fixture, required for fault')
    parser.add_argument('--target', help='postgres, redis, minio, or an owned kind worker node')
    parser.add_argument('--seconds', type=int, default=20)
    args = parser.parse_args()
    os.umask(0o077)
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    if args.action == 'fault':
        if not args.target or not 1 <= args.seconds <= 120:
            parser.error('fault requires --target and --seconds between 1 and 120')
    elif not args.backup:
        parser.error('--backup is required')
    if args.action == 'verify':
        verify_backup(args.backup)
        print('Backup checksums passed (use backup for a database restore drill).')
        return
    operations = Operations(LocalCluster(args.cluster))
    if args.action == 'fault':
        operations.fault(args.target, args.seconds, args.probe_host)
    elif args.action == 'restore':
        for name in ('worker', 'control-plane'):
            if operations.deployment(name)['spec']['replicas'] != 0:
                raise ValueError('Stop applications before restoring; they remain stopped after restore.')
        operations.restore_empty(args.backup)
        # All applications are still stopped and data + current secrets verified.
        operations.sql('UPDATE platform_maintenance SET owner=NULL WHERE singleton')
        print('Restore verified. Review and restore application replica counts explicitly.')
    else:
        getattr(operations, args.action)(args.backup)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f'Error: {error}', file=sys.stderr)
        sys.exit(1)
