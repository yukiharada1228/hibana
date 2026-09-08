"""Safety regressions for maintenance tools: no live infrastructure required."""
import json
from pathlib import Path
import tempfile
import subprocess
import time
import io
from contextlib import nullcontext
import unittest
from unittest.mock import patch

import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sdk/platform"))
from kubernetes import LocalCluster, LOCAL
from k8s_resilience import Operations, sha, verify_backup
from bounded_process import run as run_bounded


class BackupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        for name in ('postgres.dump', 'secrets.json', 'object-00000000'):
            (self.folder / name).write_bytes(b'fixture')
        self.manifest = {'format': 1, 'postgres_sha256': sha(self.folder / 'postgres.dump'),
                         'secrets_sha256': sha(self.folder / 'secrets.json'),
                         'objects': {'path/to/artifact': {'file': 'object-00000000', 'sha256': sha(self.folder / 'object-00000000')}},
                         'counts': {t: 0 for t in ('tenants', 'components', 'component_versions', 'function_secrets', 'function_secret_versions', 'executions')}}
        self.save()

    def save(self):
        (self.folder / 'manifest.json').write_text(json.dumps(self.manifest))

    def test_versioned_environment_counts_are_preserved(self):
        self.manifest['counts'].update(version_configs=2, version_secret_bindings=1)
        self.save()
        self.assertEqual(verify_backup(self.folder)['counts']['version_configs'], 2)
        del self.manifest['counts']['version_secret_bindings']
        self.save()
        with self.assertRaisesRegex(ValueError, 'table'):
            verify_backup(self.folder)

    def test_corruption_prevents_restore(self):
        verify_backup(self.folder)
        (self.folder / 'postgres.dump').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            verify_backup(self.folder)

    def test_paths_and_table_names_are_not_trusted(self):
        self.manifest['objects']['path/to/artifact']['file'] = '../outside'
        self.save()
        with self.assertRaisesRegex(ValueError, 'filename'):
            verify_backup(self.folder)
        self.manifest['objects'] = {}
        self.manifest['counts']['users; DROP DATABASE hibana'] = 1
        self.save()
        with self.assertRaisesRegex(ValueError, 'table'):
            verify_backup(self.folder)

    def test_incomplete_or_nonempty_targets_are_not_overwritten(self):
        (self.folder / 'manifest.json').unlink()
        with self.assertRaises(FileNotFoundError):
            verify_backup(self.folder)
        self.save()
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'sql', return_value='10'), patch.object(operations, 'call') as call:
            with self.assertRaisesRegex(ValueError, 'not empty'):
                operations.restore_empty(self.folder)
            call.assert_not_called()

    def test_maintenance_restores_both_replicas_after_failure(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'call', return_value=b'{"items":[]}'), \
             patch.object(operations, 'deployment', return_value={'spec': {'replicas': 2}}), \
             patch.object(operations, 'close_admission'), patch.object(operations, 'open_admission'), \
             patch.object(operations, 'drain'), patch.object(operations, 'prepare_apps'), patch.object(operations, 'scale') as scale:
            with self.assertRaisesRegex(ValueError, 'fixture'):
                with operations.stopped_apps():
                    raise ValueError('fixture')
            self.assertEqual([c.args for c in scale.call_args_list],
                             [('worker', 0), ('control-plane', 0), ('control-plane', 2), ('worker', 2)])

    def test_failed_drain_never_stops_active_workers(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'call', return_value=b'{"items":[]}'), \
             patch.object(operations, 'deployment', return_value={'spec': {'replicas': 2}}), \
             patch.object(operations, 'close_admission'), patch.object(operations, 'open_admission') as reopen, \
             patch.object(operations, 'drain', side_effect=ValueError('drain timeout')), \
             patch.object(operations, 'prepare_apps'), patch.object(operations, 'scale') as scale:
            with self.assertRaisesRegex(ValueError, 'drain timeout'):
                with operations.stopped_apps():
                    self.fail('must not snapshot')
            self.assertEqual([c.args for c in scale.call_args_list], [('control-plane', 2), ('worker', 2)])
            reopen.assert_called_once()

    def test_drain_waits_for_durable_worker_completion(self):
        operations = Operations.__new__(Operations)
        pods = json.dumps({'items': [{'metadata': {'uid': 'cp-id', 'name': 'cp'}}]}).encode()
        with patch.object(operations, 'credentials', return_value=({}, {'BOOTSTRAP_ADMIN_TOKEN': 'fixture'})), \
             patch.object(operations, 'call', return_value=pods), \
             patch.object(operations, 'forward', return_value=nullcontext('http://unused')), \
             patch('k8s_resilience.urlopen', side_effect=lambda *a, **kw: io.BytesIO(b'{"active_requests":0}')), \
             patch.object(operations, 'sql', side_effect=['1', '0']) as sql, \
             patch('k8s_resilience.time.sleep') as sleep:
            operations.drain()
        self.assertEqual(sql.call_count, 2, 'zero public requests alone cannot complete drain')
        sleep.assert_called_once()

    def test_preparation_failure_never_reopens_admission(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'call', return_value=b'{"items":[]}'), \
             patch.object(operations, 'deployment', return_value={'spec': {'replicas': 2}}), \
             patch.object(operations, 'close_admission'), patch.object(operations, 'open_admission') as reopen, \
             patch.object(operations, 'drain'), patch.object(operations, 'scale'), \
             patch.object(operations, 'prepare_apps', side_effect=ValueError('admission remains closed')):
            with self.assertRaisesRegex(ValueError, 'remains closed'):
                with operations.stopped_apps():
                    pass
            reopen.assert_not_called()

    def test_preparation_rechecks_replaced_or_restarted_workers(self):
        operations = Operations.__new__(Operations)
        old = {'a': ('10.0.0.1', 0), 'b': ('10.0.0.2', 0)}
        replacement = {'a': ('10.0.0.1', 1), 'c': ('10.0.0.3', 0)}
        def response(*args, **kwargs):
            result = io.BytesIO(b'')
            result.status = 204
            return result
        with patch.object(operations, 'credentials', return_value=({}, {'BOOTSTRAP_ADMIN_TOKEN': 'fixture'})), \
             patch.object(operations, 'forward', return_value=nullcontext('http://unused')), \
             patch.object(operations, 'ready_workers', side_effect=[old, replacement, replacement, replacement]), \
             patch('k8s_resilience.urlopen', side_effect=response) as send, \
             patch('k8s_resilience.time.sleep'):
            operations.prepare_apps('owner', 2)
            self.assertEqual(send.call_count, 2)
            self.assertEqual(json.loads(send.call_args.args[0].data)['workers'], ['10.0.0.1', '10.0.0.3'])

    def test_preparation_retries_only_503_with_a_deadline(self):
        from urllib.error import HTTPError
        operations = Operations.__new__(Operations)
        for status in (403, 409, 503):
            with self.subTest(status=status), \
                 patch.object(operations, 'credentials', return_value=({}, {'BOOTSTRAP_ADMIN_TOKEN': 'fixture'})), \
                 patch.object(operations, 'forward', return_value=nullcontext('http://unused')), \
                 patch.object(operations, 'ready_workers', return_value={'a': ('10.0.0.1', 0)}), \
                 patch('k8s_resilience.urlopen', side_effect=HTTPError('http://unused', status, 'fixture', {}, None)) as send, \
                 patch('k8s_resilience.time.sleep'), \
                 patch('k8s_resilience.time.monotonic', side_effect=range(100)):
                with self.assertRaisesRegex(ValueError, 'admission remains closed'):
                    operations.prepare_apps('owner', 1, timeout=8)
                self.assertEqual(send.call_count, 2 if status == 503 else 1)

    def test_unready_or_missing_workers_cannot_complete_preparation(self):
        operations = Operations.__new__(Operations)
        pod = {'metadata': {'uid': 'a'}, 'status': {'podIP': '10.0.0.1',
               'conditions': [{'type': 'Ready', 'status': 'False'}]}}
        with patch.object(operations, 'call', return_value=json.dumps({'items': [pod]}).encode()):
            with self.assertRaisesRegex(ValueError, 'not ready'):
                operations.ready_workers(1)
            pod['status']['conditions'][0]['status'] = 'True'
        with patch.object(operations, 'call', return_value=json.dumps({'items': [pod]}).encode()):
            with self.assertRaisesRegex(ValueError, 'replica count'):
                operations.ready_workers(2)

    def test_changed_maintenance_owner_is_not_reported_as_reopened(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'sql', return_value=''):
            with self.assertRaisesRegex(ValueError, 'ownership changed'):
                operations.open_admission('owner')

    def test_failed_storage_restore_keeps_admission_closed(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'call', return_value=b'{"items":[]}'), \
             patch.object(operations, 'deployment', return_value={'spec': {'replicas': 2}}), \
             patch.object(operations, 'close_admission'), patch.object(operations, 'open_admission') as reopen, \
             patch.object(operations, 'drain'), patch.object(operations, 'scale') as scale:
            with operations.stopped_apps() as counts:
                counts.update({'control-plane': 0, 'worker': 0})
            reopen.assert_not_called()
            self.assertTrue(all(c.args[1] == 0 for c in scale.call_args_list))

    def test_rotated_key_ids_are_required_and_checksummed(self):
        self.manifest['format'] = 2
        path = self.folder / 'key-config.json'
        path.write_text(json.dumps({'SECRETS_MASTER_KID': 'rotated-k2', 'JOB_SIGNING_KID': 'sign-k2'}))
        self.manifest['key_config_sha256'] = sha(path)
        self.save()
        verify_backup(self.folder)
        path.write_text(path.read_text().replace('rotated-k2', 'k1'))
        with self.assertRaisesRegex(ValueError, 'checksum'):
            verify_backup(self.folder)

    def test_snapshot_preserves_rotated_key_ids_without_source_endpoints(self):
        operations = Operations.__new__(Operations)
        operations.cluster = type('Cluster', (), {'name': 'test'})()
        config = {'SECRETS_MASTER_KID': 'rotated-k2', 'JOB_SIGNING_KID': 'sign-k3',
                  'S3_BUCKET': 'test', 'DATABASE_URL': 'must-not-copy'}
        def call(*args, **kwargs):
            if 'output' in kwargs:
                kwargs['output'].write(b'dump')
            return json.dumps({'data': config}).encode()
        with patch.object(operations, 'call', side_effect=call), \
             patch.object(operations, 'credentials', return_value=({'items': []}, {'S3_ACCESS_KEY': 'test', 'S3_SECRET_KEY': 'test'})), \
             patch.object(operations, 'deployment', return_value={'spec': {'template': {'spec': {'containers': [{'envFrom': [
                 {'configMapRef': {'name': 'hibana-config'}}, {'secretRef': {'name': 'hibana-runtime'}},
                 {'secretRef': {'name': 'hibana-control-plane'}}]}]}}}}), \
             patch.object(operations, 'forward', return_value=nullcontext('http://unused')), \
             patch.object(operations, 'sql', return_value='0'), patch('k8s_resilience.S3') as s3:
            s3.return_value.keys.return_value = []
            folder = self.folder / 'snapshot'
            manifest = operations.snapshot(folder)
        self.assertEqual(manifest['format'], 2)
        self.assertEqual(json.loads((folder / 'key-config.json').read_text()),
                         {'SECRETS_MASTER_KID': 'rotated-k2', 'JOB_SIGNING_KID': 'sign-k3'})
        verify_backup(folder)

    def test_hanging_process_tree_is_reaped_before_fault_recovery(self):
        marker = self.folder / 'ticks'
        child = "import signal,time; from pathlib import Path; signal.signal(signal.SIGTERM, signal.SIG_IGN); p=Path(" + repr(str(marker)) + ");\nwhile True:\n p.open('a').write('x'); time.sleep(.01)"
        parent = 'import subprocess,signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); subprocess.Popen([' + repr(sys.executable) + ',"-c",' + repr(child) + ']); time.sleep(60)'
        began = time.monotonic()
        recovered = False
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                run_bounded([sys.executable, '-c', parent], timeout=.5)
        finally:
            # Mirrors fault unpause: cleanup has an independent finite budget.
            self.assertEqual(run_bounded([sys.executable, '-c', 'print("resumed")'], timeout=2), b'resumed\n')
            recovered = True
        self.assertTrue(recovered)
        self.assertLess(time.monotonic() - began, 4)
        before = marker.read_text()
        self.assertTrue(before)
        time.sleep(.1)
        self.assertEqual(marker.read_text(), before)

    def test_hpa_refuses_maintenance_before_mutation(self):
        operations = Operations.__new__(Operations)
        with patch.object(operations, 'call', return_value=b'{"items":[{}]}'), patch.object(operations, 'scale') as scale:
            with self.assertRaisesRegex(ValueError, 'HPA'):
                with operations.stopped_apps():
                    self.fail('must not enter maintenance')
            scale.assert_not_called()

    def test_startup_preserves_existing_storage_and_uses_pvc_for_new_clusters(self):
        cluster = LocalCluster()
        def doc(volume):
            return {'spec': {'template': {'spec': {'volumes': [volume]}}}}
        with patch.object(cluster, 'kube', return_value='{"items":[]}'):
            self.assertEqual(cluster.dependency_path(), LOCAL.parent / 'persistent-dependencies')
        with patch.object(cluster, 'kube', return_value=json.dumps({'items': [doc({'emptyDir': {}})]})):
            self.assertEqual(cluster.dependency_path(), LOCAL / 'dependencies')
        with patch.object(cluster, 'kube', return_value=json.dumps({'items': [doc({'persistentVolumeClaim': {}})]})):
            self.assertEqual(cluster.dependency_path(), LOCAL.parent / 'persistent-dependencies')
        with patch.object(cluster, 'kube', return_value=json.dumps({'items': [doc({'persistentVolumeClaim': {}}), doc({'emptyDir': {}})]})):
            with self.assertRaisesRegex(ValueError, 'Mixed'):
                cluster.dependency_path()


if __name__ == '__main__':
    unittest.main()
