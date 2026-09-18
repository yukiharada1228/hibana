"""Local lifecycle concurrency regressions; all Docker/Kubernetes calls are fake."""
from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import kubernetes as k8s
from local_operation import LocalOperationLock

PLATFORM = Path(__file__).resolve().parent
COMPETITOR = """
import sys
from pathlib import Path
from unittest.mock import patch
from kubernetes import LocalCluster
cluster = LocalCluster('fixture')
cluster.state = Path(sys.argv[1])
def unexpected(*args, **kwargs):
    raise AssertionError('competing operation reached cluster access')
try:
    with patch.object(cluster, 'owned_nodes', unexpected), patch.object(cluster, 'preflight', unexpected):
        getattr(cluster, sys.argv[2])()
except ValueError as error:
    print(error)
    sys.exit(2)
"""


class LocalOperationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.state = Path(temporary.name)
        self.cluster = k8s.LocalCluster("fixture")
        self.cluster.state = self.state

    def competitor(self, action):
        result = subprocess.run([sys.executable, "-c", COMPETITOR, str(self.state), action],
                                cwd=PLATFORM, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("Another local platform operation holds the lock", result.stdout)

    def test_stop_blocks_other_processes_until_nodes_stop_and_owner_stays_restartable(self):
        draining, finish = threading.Event(), threading.Event()
        state = {"running": True, "owner": None}
        failures = []
        def nodes():
            return [{"Id": "fixture", "Name": "/fixture-control-plane",
                     "State": {"Running": state["running"]}}]
        def close(owner):
            self.assertIn(state["owner"], (None, owner))
            state["owner"] = owner
        def open_gate(owner):
            self.assertEqual(state["owner"], owner)
            state["owner"] = None
        def drain():
            draining.set()
            if not finish.wait(timeout=15):
                raise AssertionError("test drain was not released")
        def run(*args, **kwargs):
            if args[:2] == ("docker", "stop"):
                self.assertIsNotNone(state["owner"], "admission must remain closed through node stop")
                state["running"] = False
            else:
                self.assertEqual(args[:2], ("docker", "start"))
                state["running"] = True
        def stop():
            try:
                self.cluster.stop()
            except BaseException as error:
                failures.append(error)

        with patch.object(self.cluster, "owned_nodes", side_effect=nodes), \
                patch.object(self.cluster, "kube"), patch.object(self.cluster, "wait_workloads"), \
                patch("kubernetes.run", side_effect=run), patch("kubernetes.Maintenance") as maintenance, \
                redirect_stdout(io.StringIO()):
            maintenance.return_value.close.side_effect = close
            maintenance.return_value.open.side_effect = open_gate
            maintenance.return_value.drain.side_effect = drain
            thread = threading.Thread(target=stop)
            thread.start()
            try:
                self.assertTrue(draining.wait(timeout=5))
                saved = (self.state / "maintenance.json").read_bytes()
                for action in ("start", "stop", "install", "uninstall"):
                    self.competitor(action)
                    self.assertEqual((self.state / "maintenance.json").read_bytes(), saved)
                maintenance.return_value.open.assert_not_called()
            finally:
                finish.set()
                thread.join(timeout=5)
            self.assertFalse(thread.is_alive())
            self.assertEqual(failures, [])
            self.assertFalse(state["running"])
            self.assertFalse((self.state / "operation.lock").exists())
            self.assertEqual(json.loads(saved)["owner"], state["owner"])
            self.cluster.start()
            maintenance.return_value.prepare.assert_called_once_with(json.loads(saved)["owner"])
            maintenance.return_value.open.assert_called_once_with(json.loads(saved)["owner"])
            self.assertTrue(state["running"])
            self.assertFalse((self.state / "maintenance.json").exists())

    def test_each_mutation_holds_lock_before_cluster_access_and_releases_after_failure(self):
        for action in ("start", "stop", "install", "uninstall"):
            def fail():
                record, = (self.state / "operation.lock").iterdir()
                self.assertEqual(json.loads(record.read_text())["action"], action)
                self.competitor("start")
                raise ValueError("fixture failure")
            first = "preflight" if action == "install" else "owned_nodes"
            with self.subTest(action=action), patch.object(self.cluster, first, side_effect=fail):
                with self.assertRaisesRegex(ValueError, "fixture failure"):
                    getattr(self.cluster, action)()
            self.assertFalse((self.state / "operation.lock").exists())

    def test_install_resumes_stopped_nodes_under_its_original_lock(self):
        self.cluster.kubeconfig = self.state / "kubeconfig"
        self.cluster.kubeconfig.touch()
        config = k8s.yaml.safe_load((k8s.LOCAL / "kind.yaml").read_text())
        bindings = {f'{p["containerPort"]}/tcp': [{"HostIp": p["listenAddress"], "HostPort": str(p["hostPort"])}]
                    for p in config["nodes"][0]["extraPortMappings"]}
        def run(*args, **kwargs):
            if args[1:3] == ("get", "clusters"):
                return "fixture\n"
            if args[:2] == ("docker", "inspect"):
                return json.dumps(bindings)
            self.assertEqual(args, ("docker", "start", "fixture"))
            self.competitor("stop")
        def ensure_then_fail():
            self.cluster.ensure_cluster()
            raise ValueError("end of fixture")
        with patch.object(self.cluster, "preflight", side_effect=ensure_then_fail), \
                patch.object(self.cluster, "owned_nodes", return_value=[{"Id": "fixture", "State": {"Running": False}}]), \
                patch.object(self.cluster, "kube"), patch.object(self.cluster, "resume_admission") as resume, \
                patch("kubernetes.run", side_effect=run), patch("kubernetes.shutil.which", return_value="tool"):
            with self.assertRaisesRegex(ValueError, "end of fixture"):
                self.cluster.install()
            resume.assert_not_called()
        self.assertFalse((self.state / "operation.lock").exists())

    def test_exception_and_interrupt_release_only_operation_lock(self):
        saved = self.state / "maintenance.json"
        saved.write_text('{"owner":"fixture"}')
        for error in (ValueError("fixture"), KeyboardInterrupt()):
            with self.assertRaises(type(error)):
                with self.cluster.operation("stop"):
                    raise error
            with self.cluster.operation("start"):
                self.assertEqual(saved.read_text(), '{"owner":"fixture"}')

    def test_cli_sigterm_releases_lock(self):
        handlers = {}
        def terminate():
            handlers[signal.SIGTERM](signal.SIGTERM, None)
        with patch("kubernetes.sys.argv", ["kubernetes.py", "stop"]), \
                patch("kubernetes.signal.signal", side_effect=lambda sig, handler: handlers.update({sig: handler})), \
                patch("kubernetes.os.umask"), patch("kubernetes.LocalCluster", return_value=self.cluster), \
                patch.object(self.cluster, "owned_nodes", side_effect=terminate), \
                redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            self.assertEqual(k8s.main(), 130)
        self.assertFalse((self.state / "operation.lock").exists())

    def test_forced_process_exit_does_not_release_or_expire_lock(self):
        script = """
import os, sys
from pathlib import Path
from local_operation import LocalOperationLock
with LocalOperationLock(Path(sys.argv[1]), 'stop'):
    os._exit(23)
"""
        result = subprocess.run([sys.executable, "-c", script, str(self.state)], cwd=PLATFORM, timeout=10)
        self.assertEqual(result.returncode, 23)
        record, = (self.state / "operation.lock").iterdir()
        content = json.loads(record.read_text())
        content["started_at"] = "2000-01-01T00:00:00Z"
        record.write_text(json.dumps(content))
        self.competitor("start")
        self.assertEqual(json.loads(record.read_text()), content)
        # Explicit recovery, after the original process and children have ended.
        record.unlink()
        record.parent.rmdir()
        with self.cluster.operation("start"):
            pass

    def test_stale_release_cannot_remove_replacement_lock(self):
        old = self.cluster.operation("stop").__enter__()
        old.record.unlink()
        old.path.rmdir()
        with self.cluster.operation("start") as replacement:
            before = replacement.record.read_bytes()
            with self.assertRaisesRegex(ValueError, "Could not release"):
                old.__exit__(None, None, None)
            self.assertEqual(replacement.record.read_bytes(), before)

    def test_status_preview_and_separate_cluster_do_not_take_this_lock(self):
        with self.cluster.operation("stop"):
            with LocalOperationLock(self.state / "another-cluster", "install"):
                pass
            with patch.object(self.cluster, "owned_nodes", return_value=[
                    {"Name": "/fixture-control-plane", "State": {"Running": False}}]), \
                    redirect_stdout(io.StringIO()):
                self.cluster.status()
                self.cluster.preview("start")


if __name__ == "__main__":
    unittest.main()
