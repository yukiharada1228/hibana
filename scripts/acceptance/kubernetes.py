#!/usr/bin/env python3
"""MVP acceptance on a new, checkout-owned kind cluster; never a default context.

The upstream uses a synthetic public address on an INTERNAL Docker network. This
lets the real public-IP egress gate run unchanged without traffic to the Internet.
The address route exists only in the disposable node containers, not on the host.
"""
import argparse
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import secrets
import sys
import threading
import time
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'scripts'), str(ROOT / 'sdk/platform')]
from bounded_process import run
from kubernetes import LocalCluster, LOCAL, write_private
from k8s_resilience import Operations
from results import summarize_resources
import yaml

CLUSTER = 'hibana-mvp-check'
NETWORK = 'hibana-mvp-upstream'
UPSTREAM = 'hibana-mvp-inventory'


def command(*args, timeout=60, input=None):
    return run([str(a) for a in args], timeout=timeout, input=input).decode().strip()


def observe(operations, folder, stop, endpoints, node):
    while not stop.is_set():
        sample = {'at': datetime.now(timezone.utc).isoformat()}
        try:
            pods = json.loads(operations.call('get', 'pods', '-l', 'app.kubernetes.io/name=hibana-worker', '-o', 'json'))['items']
            sample['workers'] = []
            for pod in pods:
                if stop.is_set():
                    return  # Do not publish an incomplete final fleet observation.
                name = pod['metadata']['name']
                item = {'name': name, 'uid': pod['metadata']['uid'],
                        'restarts': sum(c['restartCount'] for c in pod.get('status', {}).get('containerStatuses', []))}
                item['memory_bytes'] = int(operations.call('exec', name, '--', 'cat', '/sys/fs/cgroup/memory.current'))
                item['cache_kib'] = int(operations.call('exec', name, '--', 'du', '-sk', '/var/cache/hibana').split()[0])
                item['cpu_stat'] = operations.call('exec', name, '--', 'cat', '/sys/fs/cgroup/cpu.stat').decode().strip()
                item['cpu_pressure'] = operations.call('exec', name, '--', 'cat', '/sys/fs/cgroup/cpu.pressure').decode().strip()
                with urlopen(endpoints[name] + '/metrics', timeout=5) as response:
                    metrics = response.read(1024 * 1024).decode()
                # These metric families contain only fixed operation/outcome labels.
                item['internal_requests'] = [line for line in metrics.splitlines()
                    if line.startswith('hibana_worker_control_plane_request_duration_seconds')]
                sample['workers'].append(item)
            sample['vm'] = command('docker', 'exec', node, 'cat', '/proc/loadavg', '/proc/pressure/cpu', '/proc/pressure/io', '/proc/pressure/memory')
            sample['database'] = json.loads(operations.sql("SELECT json_build_object('bytes',pg_database_size(current_database()),'connections',(SELECT count(*) FROM pg_stat_activity),'pending',count(*) FILTER (WHERE status='pending'),'running',count(*) FILTER (WHERE status='running'),'executions',count(*)) FROM executions"))
        except Exception as error:
            sample['observation_error'] = type(error).__name__
        with (folder / 'resources.jsonl').open('a') as output:
            output.write(json.dumps(sample) + '\n')
        stop.wait(30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    global CLUSTER, NETWORK, UPSTREAM
    parser.add_argument('--seconds', type=int, default=7200)
    parser.add_argument('--cluster', default=CLUSTER)
    parser.add_argument('--image', default='hibana-platform:mvp-acceptance')
    parser.add_argument('--cli-entry', type=Path, default=ROOT / 'sdk/src/cli.mjs')
    args = parser.parse_args()
    if not 30 <= args.seconds <= 86400:
        raise ValueError('seconds must be 30..86400')
    os.chdir(ROOT)
    cluster = LocalCluster(args.cluster)
    if args.cluster != CLUSTER:
        NETWORK, UPSTREAM = args.cluster + '-upstream', args.cluster + '-inventory'
    CLUSTER = args.cluster
    args.cli_entry = args.cli_entry.resolve()
    if not args.cli_entry.is_file():
        raise ValueError('CLI entry must be an installed hibana src/cli.mjs file')
    if CLUSTER in command(cluster.kind, 'get', 'clusters').splitlines():
        raise ValueError('Acceptance cluster already exists; inspect it before a new run')
    if command('docker', 'network', 'ls', '--filter', f'name=^{NETWORK}$', '--format', '{{.ID}}'):
        raise ValueError('Acceptance network already exists; no changes made')
    folder = ROOT / '.local/mvp-acceptance' / datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    folder.mkdir(parents=True, mode=0o700)
    cluster.state.mkdir(parents=True, exist_ok=True, mode=0o700)
    kind_config = yaml.safe_load((LOCAL / 'kind.yaml').read_text())
    for node in kind_config['nodes']:
        node.pop('extraPortMappings', None)
    config_path = folder / 'kind.yaml'
    config_path.write_text(yaml.safe_dump(kind_config))
    created = network = upstream = False
    report = {'started_at': datetime.now(timezone.utc).isoformat(), 'cluster': CLUSTER,
              'requested_seconds': args.seconds, 'passed': False, 'scope': 'single-host kind; synthetic upstream; real PostgreSQL/Redis/MinIO'}
    try:
        print('Creating isolated kind cluster without fixed host ports', flush=True)
        created = True  # We claimed an absent name; also clean a partial kind creation.
        command(cluster.kind, 'create', 'cluster', '--name', CLUSTER, '--config', config_path,
                '--kubeconfig', cluster.kubeconfig, '--wait', '120s', timeout=240)
        identity = command('docker', 'image', 'inspect', args.image, '--format', '{{.Id}}')
        if not identity.startswith('sha256:') or len(identity) != 71:
            raise ValueError('Unexpected image identity')
        image = 'hibana-platform:acceptance-' + identity.split(':')[1]
        command('docker', 'tag', args.image, image)
        report['image'] = image
        report['image_id'] = identity
        command(cluster.kind, 'load', 'docker-image', image, '--name', CLUSTER, timeout=180)
        credentials = cluster.credentials()
        cluster.apply([d for d in cluster.render(LOCAL, image) if d['kind'] == 'Namespace'])
        cluster.apply(credentials['items'])
        cluster.kube('apply', '-k', cluster.dependency_path())
        for dep in ('postgres', 'redis', 'minio'):
            cluster.kube('-n', 'hibana', 'rollout', 'status', f'deployment/hibana-{dep}', '--timeout=240s')
        cluster.kube('-n', 'hibana', 'wait', '--for=condition=complete', 'job/hibana-local-bucket', '--timeout=240s')
        print('Installing through hibana platform with an explicit Kubernetes context', flush=True)
        command('node', args.cli_entry, 'platform', 'install', '--kubeconfig', cluster.kubeconfig,
                '--context', f'kind-{CLUSTER}', '--overlay', LOCAL, '--image', image, timeout=900)
        operations = Operations(cluster)
        report['pod_images'] = json.loads(operations.call('get', 'pods', '-o', 'json'))['items']
        report['pod_images'] = [{'name': p['metadata']['name'], 'images': [c.get('imageID') for c in p.get('status', {}).get('containerStatuses', [])]}
                                for p in report['pod_images'] if p['metadata']['labels'].get('app.kubernetes.io/name') in ('hibana-worker', 'hibana-control-plane')]
        # A private Docker bridge emulates an Internet-addressed dependency. No port
        # is published, and only the owned kind nodes are connected to this bridge.
        command('docker', 'network', 'create', '--internal', '--subnet', '11.203.42.0/24', NETWORK)
        network = True
        for node in cluster.owned_nodes():
            command('docker', 'network', 'connect', NETWORK, node['Id'])
        env_file = folder / 'upstream.env'
        upstream_token = secrets.token_hex(32)
        write_private(env_file, 'UPSTREAM_TOKEN=' + upstream_token + '\n')
        command('docker', 'run', '-d', '--rm', '--name', UPSTREAM, '--network', NETWORK, '--ip', '11.203.42.10',
                '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges', '--pids-limit=64', '--memory=128m',
                '--env-file', env_file, '-v', f'{ROOT / "scripts/acceptance/upstream.mjs"}:/upstream.mjs:ro',
                'node:24-bookworm-slim', 'node', '/upstream.mjs')
        upstream = True
        # Keep the overlay narrow even on a CNI which enforces NetworkPolicy.
        cluster.apply([{'apiVersion':'networking.k8s.io/v1','kind':'NetworkPolicy',
                        'metadata':{'name':'acceptance-upstream','namespace':'hibana'},
                        'spec':{'podSelector':{'matchLabels':{'app.kubernetes.io/name':'hibana-worker'}},
                                'policyTypes':['Egress'],'egress':[{'to':[{'ipBlock':{'cidr':'11.203.42.10/32'}}],
                                                                 'ports':[{'protocol':'TCP','port':8080}]}]}}])
        with ExitStack() as stack:
            worker_pods = json.loads(operations.call('get', 'pods', '-l', 'app.kubernetes.io/name=hibana-worker', '-o', 'json'))['items']
            endpoints = {p['metadata']['name']: stack.enter_context(operations.forward('worker', 9090, resource='pod/' + p['metadata']['name'])) for p in worker_pods}
            api = stack.enter_context(operations.forward('control-plane', 8080))
            apps = stack.enter_context(operations.forward('control-plane', 8083))
            client_env = cluster.sdk_env()
            client_env.update(HIBANA_URL=api, GATEWAY=apps, HIBANA_UPSTREAM_TOKEN=upstream_token,
                              HIBANA_UPSTREAM_URL='http://11.203.42.10:8080', HIBANA_ACCEPTANCE_FOLDER=str(folder),
                              HIBANA_ACCEPTANCE_SECONDS=str(args.seconds), HIBANA_CLI_ENTRY=str(args.cli_entry),
                              HIBANA_CONFIG_HOME=str(folder / 'cli-settings'), HIBANA_PROFILE='')
            # No credentials in argv or report. Child environment is confined to this process.
            previous = os.environ.copy()
            os.environ.update(client_env)
            stop_observer = threading.Event()
            observer = threading.Thread(target=observe, args=(operations, folder, stop_observer, endpoints, CLUSTER + '-control-plane'), daemon=True)
            observer.start()
            try:
                print('Running CLI application acceptance and sustained HTTP load', flush=True)
                with (folder / 'application.log').open('wb') as log:
                    try:
                        run(['node', str(ROOT / 'scripts/acceptance/application.mjs')], stdout=log,
                            timeout=args.seconds + 1200)
                    except ValueError:
                        # A measured load failure must not hide the independent
                        # recovery result. Setup/functional failures still abort.
                        result = folder / 'application.json'
                        if not result.exists() or json.loads(result.read_text()).get('passed') is not False:
                            raise
                        print('HTTP load failed; continuing with independent backup verification', flush=True)
            finally:
                stop_observer.set()
                observer.join(timeout=180)
                os.environ.clear()
                os.environ.update(previous)
                if observer.is_alive():
                    raise ValueError('Resource observer did not finish within its cleanup budget')
        # Capture stable failure categories before maintenance changes Pods or state.
        try:
            categories = operations.sql("SELECT coalesce(json_object_agg(reason,n),'{}'::json) FROM (SELECT CASE WHEN error::text LIKE '%secret material unavailable: timeout%' THEN 'secret_timeout' WHEN error::text LIKE '%secret material unavailable: connect%' THEN 'secret_connect' WHEN error::text LIKE '%secret material unavailable:%' THEN 'secret_other' WHEN error::text LIKE '%dispatch%' THEN 'dispatch' ELSE 'other' END reason,count(*) n FROM executions WHERE status IN ('failed','timeout') GROUP BY 1) failures")
            report['execution_failure_classes'] = json.loads(categories)
        except Exception as error:
            # Diagnostics must not suppress the independent recovery exercise.
            report['diagnostic_error'] = type(error).__name__
        print('Verifying DB/S3/key backup and restoring a separate temporary database', flush=True)
        operations.backup(folder / 'backup')
        report['backup_restore'] = 'custom dump restored to separate DB; counts and actual Secret decryption verified; S3 backup hashes verified'
        report['post_maintenance_http'] = False
        # New port-forward connections after drain/Pod replacement.
        with operations.forward('control-plane', 8080) as api, operations.forward('control-plane', 8083) as apps:
            state = json.loads((folder / 'client.json').read_text())
            state.update(url=api, gateway=apps)
            # write_private deliberately refuses existing paths. Publish the new
            # endpoints atomically without weakening private file permissions.
            next_client = folder / 'client.next.json'
            write_private(next_client, json.dumps(state))
            next_client.replace(folder / 'client.json')
            command('node', ROOT / 'scripts/acceptance/application.mjs', '--verify-restored', folder, timeout=180)
        report['post_maintenance_http'] = True
        report['recovery'] = json.loads((folder / 'recovery.json').read_text())
        report['application'] = json.loads((folder / 'application.json').read_text())
        report['version_code'] = json.loads((folder / 'version-code.json').read_text())
        observations = [json.loads(line) for line in (folder / 'resources.jsonl').read_text().splitlines()]
        report['resource_samples'] = len(observations)
        report['resource_observation_errors'] = sum('observation_error' in row for row in observations)
        report['worker_restarts'] = max((worker['restarts'] for row in observations for worker in row.get('workers', [])), default=-1)
        if report['resource_observation_errors'] or report['worker_restarts'] != 0:
            raise ValueError('Resource observations were incomplete or a Worker restarted')
        report['resources'] = summarize_resources(observations, minimum_seconds=report['application']['elapsed_seconds'])
        print('Verifying CLI platform stop/start, application deletion and platform uninstall', flush=True)
        def platform(action):
            command('node', args.cli_entry, 'platform', action, '--kubeconfig', cluster.kubeconfig,
                    '--context', f'kind-{CLUSTER}', *(['--yes'] if action == 'uninstall' else []), timeout=900)

        def dependencies_preserved():
            for dependency in ('postgres', 'redis', 'minio'):
                if operations.deployment(dependency).get('status', {}).get('readyReplicas', 0) < 1:
                    raise ValueError('External dependency was stopped or removed')

        drain = {}
        with operations.forward('control-plane', 8083) as apps:
            state = json.loads((folder / 'client.json').read_text())
            def slow_request():
                try:
                    request = Request(apps + '/items/DRAIN', headers={
                        'Host': 'inventory-api.smoke.hibana.local',
                        'Authorization': 'Bearer ' + state['apiToken']})
                    with urlopen(request, timeout=20) as response:
                        drain['status'] = response.status
                        drain['body_valid'] = json.load(response) == {'sku':'DRAIN','name':'Drain check','available':1}
                    drain['gate_closed_at_completion'] = operations.sql('SELECT owner IS NOT NULL FROM platform_maintenance WHERE singleton') == 't'
                except Exception as error:
                    drain['error'] = type(error).__name__
            request_thread = threading.Thread(target=slow_request)
            request_thread.start()
            try:
                deadline = time.monotonic() + 5
                execution = ''
                while time.monotonic() < deadline and not execution:
                    execution = operations.sql("SELECT id FROM executions WHERE status='running' ORDER BY created_at DESC LIMIT 1")
                    if not execution:
                        time.sleep(0.1)
                if not execution:
                    raise ValueError('Slow guest did not enter execution before the drain check')
                platform('stop')
            finally:
                request_thread.join(timeout=25)
            if request_thread.is_alive() or drain != {'status':200,'body_valid':True,'gate_closed_at_completion':True}:
                raise ValueError('In-flight HTTP request did not complete during CLI platform stop')
            if operations.sql("SELECT status FROM executions WHERE id='" + execution + "'") != 'succeeded':
                raise ValueError('Worker completion was not persisted before platform stop')
        report['stop_inflight_http'] = drain
        for name in ('control-plane', 'worker'):
            deployment = operations.deployment(name)
            if deployment['spec'].get('replicas') != 0:
                raise ValueError('Platform stop left active replicas')
            pods = json.loads(operations.call('get', 'pods', '-l', f'app.kubernetes.io/name=hibana-{name}', '-o', 'json'))['items']
            if pods:
                raise ValueError('Platform stop left runtime Pods')
        dependencies_preserved()
        report['platform_stopped'] = True
        platform('start')
        # Keep the published applications in the external DB, then reinstall to
        # exercise recovery of the namespace's saved maintenance owner as well.
        platform('uninstall')
        dependencies_preserved()
        command('node', args.cli_entry, 'platform', 'install', '--kubeconfig', cluster.kubeconfig,
                '--context', f'kind-{CLUSTER}', '--overlay', LOCAL, '--image', image, timeout=900)
        if operations.sql('SELECT owner IS NULL FROM platform_maintenance WHERE singleton') != 't':
            raise ValueError('Reinstall did not reopen admission after preparing retained applications')
        report['reinstall_preserved_applications'] = True
        with operations.forward('control-plane', 8080) as api, operations.forward('control-plane', 8083) as apps:
            state = json.loads((folder / 'client.json').read_text())
            state.update(url=api, gateway=apps)
            next_client = folder / 'client.next.json'
            write_private(next_client, json.dumps(state))
            next_client.replace(folder / 'client.json')
            command('node', ROOT / 'scripts/acceptance/application.mjs', '--verify-lifecycle', folder, timeout=240)
        report['lifecycle'] = json.loads((folder / 'lifecycle.json').read_text())
        platform('uninstall')
        for name in ('hibana-control-plane', 'hibana-worker'):
            if operations.call('get', 'deployment', name, '--ignore-not-found', '-o', 'name').strip():
                raise ValueError('Platform uninstall left a runtime Deployment')
        if operations.call('get', 'configmap', 'hibana-platform', '--ignore-not-found', '-o', 'name').strip():
            raise ValueError('Platform uninstall left its installation record')
        dependencies_preserved()
        report['platform_uninstalled'] = True
        report['passed'] = report['application']['passed'] and report['version_code']['passed'] and not report.get('diagnostic_error')
        print(('PASS' if report['passed'] else 'FAIL') + f' acceptance ({report["requested_seconds"]}s load); report: ' + str(folder / 'report.json'), flush=True)
    finally:
        # Only exact resources created by this run are removed. Never stop other clusters.
        errors = []
        for owned, args in ((upstream, ['docker', 'stop', UPSTREAM]),
                            (created, [cluster.kind, 'delete', 'cluster', '--name', CLUSTER, '--kubeconfig', cluster.kubeconfig]),
                            (network, ['docker', 'network', 'rm', NETWORK])):
            if owned:
                try:
                    command(*args, timeout=180)
                except Exception as error:
                    errors.append(type(error).__name__)
        report['cleanup_errors'] = errors
        if errors:
            report['passed'] = False
        report['finished_at'] = datetime.now(timezone.utc).isoformat()
        (folder / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        if errors:
            raise ValueError('Acceptance cleanup incomplete; inspect the dedicated resources')

    if not report['passed']:
        raise ValueError('MVP acceptance failed; see the separate load and recovery results in report.json')


if __name__ == '__main__':
    main()
