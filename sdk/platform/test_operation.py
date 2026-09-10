"""Operator concurrency and drain regressions against a resourceVersion-aware API."""
from copy import copy, deepcopy
import contextlib
import io
import signal
import subprocess
import threading
import unittest
from unittest.mock import MagicMock, patch

from existing import WORKLOADS, validate
from maintenance import Maintenance
from operation import OperationLock, NAME
from test_preflight import ClusterFixture, resource


class OperationTests(ClusterFixture):
    def test_concurrent_create_and_replace_have_only_one_winner(self):
        for exists in (False, True):
            if exists:
                with OperationLock(self.cluster, "install"):
                    pass
            else:
                self.cluster.live.pop(("ConfigMap", NAME), None)
            barrier = threading.Barrier(2)
            original_get = self.cluster.get
            def snapshot(kind, name):
                value = original_get(kind, name)
                barrier.wait(timeout=5)
                return value
            acquired, failures = [], []
            def acquire():
                try:
                    acquired.append(OperationLock(self.cluster, "stop").__enter__())
                except ValueError as error:
                    failures.append(str(error))
            with patch.object(self.cluster, "get", side_effect=snapshot):
                threads = [threading.Thread(target=acquire) for _ in range(2)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join(timeout=10)
                self.assertFalse(any(t.is_alive() for t in threads))
            self.assertEqual((len(acquired), len(failures)), (1, 1))
            self.assertIn("Could not acquire", failures[0])
            acquired[0].__exit__(None, None, None)

    def test_all_mutations_refuse_a_competing_stop_and_saved_owner_stays_restartable(self):
        self.output(self.cluster.install)
        competitor = copy(self.cluster)
        competitor._operation_mutex = threading.RLock()
        competitor._operation_active = False
        draining, finish = threading.Event(), threading.Event()
        gate, errors = {}, []
        def close(owner):
            self.assertIn(gate.get("owner"), (None, owner))
            gate["owner"] = owner
        def drain():
            draining.set()
            if not finish.wait(timeout=10):
                raise AssertionError("test drain was not released")
        def stop():
            try:
                self.cluster.stop()
            except BaseException as error:
                errors.append(error)
        with patch("existing.Maintenance") as maintenance:
            maintenance.return_value.close.side_effect = close
            maintenance.return_value.drain.side_effect = drain
            thread = threading.Thread(target=stop)
            thread.start()
            try:
                self.assertTrue(draining.wait(timeout=5))
                before = self.cluster.record()
                for action in ("stop", "start", "uninstall", "install"):
                    with self.assertRaisesRegex(ValueError, "Another platform operation holds the lock"):
                        self.output(getattr(competitor, action))
                    self.assertEqual(self.cluster.record(), before)
            finally:
                finish.set()
                thread.join(timeout=10)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(self.cluster.record()["paused"]["owner"], gate["owner"])
            self.assertTrue(all(self.cluster.get("Deployment", n)["spec"]["replicas"] == 0 for n in WORKLOADS))
            self.output(competitor.start)
            maintenance.return_value.open.assert_called_once_with(gate["owner"])
            self.assertNotIn("paused", self.cluster.record())
            self.assertTrue(all(self.cluster.get("Deployment", n)["spec"]["replicas"] == 2 for n in WORKLOADS))

    def test_lock_is_released_after_failure_or_interrupt(self):
        for error in (ValueError("failure"), KeyboardInterrupt()):
            with self.assertRaises(type(error)):
                with OperationLock(self.cluster, "install"):
                    raise error
            self.assertEqual(self.cluster.get("ConfigMap", NAME)["data"], {"owner": ""})
            with OperationLock(self.cluster, "stop"):
                pass

    def test_old_lock_never_expires_and_stale_release_cannot_clear_replacement(self):
        old = OperationLock(self.cluster, "stop").__enter__()
        self.cluster.live[("ConfigMap", NAME)]["data"]["started_at"] = "2000-01-01T00:00:00Z"
        with self.assertRaisesRegex(ValueError, "forcibly terminated"):
            OperationLock(self.cluster, "start").__enter__()
        # Explicit administrator recovery after confirming the old CLI has ended.
        del self.cluster.live[("ConfigMap", NAME)]
        with OperationLock(self.cluster, "start") as replacement:
            with self.assertRaisesRegex(ValueError, "Could not release"):
                old.__exit__(None, None, None)
            self.assertEqual(self.cluster.get("ConfigMap", NAME)["data"]["owner"], replacement.acquired["data"]["owner"])

    def test_lost_acquisition_response_does_not_allow_a_second_operator(self):
        original = self.cluster.kube
        def lose_reply(*args, **kwargs):
            result = original(*args, **kwargs)
            if args[0] == "create":
                raise subprocess.TimeoutExpired(args, 30)
            return result
        with patch.object(self.cluster, "kube", side_effect=lose_reply):
            with self.assertRaisesRegex(ValueError, "Could not acquire"):
                OperationLock(self.cluster, "stop").__enter__()
        self.assertTrue(self.cluster.get("ConfigMap", NAME)["data"]["owner"])
        with self.assertRaisesRegex(ValueError, "Another platform operation"):
            OperationLock(self.cluster, "start").__enter__()

    def test_missing_release_permission_never_creates_a_lock(self):
        self.cluster.denied.add("update")
        with self.assertRaisesRegex(ValueError, "permission is missing"):
            with OperationLock(self.cluster, "stop"):
                self.fail("lock must not be acquired")
        self.assertIsNone(self.cluster.get("ConfigMap", NAME))
        self.assert_read_only()

    def test_foreign_operation_lock_is_never_replaced(self):
        foreign = resource("ConfigMap", NAME, data={"owner": "foreign"})
        foreign["metadata"]["labels"]["app.kubernetes.io/managed-by"] = "another"
        self.cluster.live[("ConfigMap", NAME)] = deepcopy(foreign)
        with self.assertRaisesRegex(ValueError, "another manager"):
            OperationLock(self.cluster, "stop").__enter__()
        self.assertEqual(self.cluster.get("ConfigMap", NAME), foreign)
        self.assert_read_only()

    def test_install_rechecks_state_after_preflight_before_changing_workloads(self):
        self.output(self.cluster.install)
        original = self.cluster.prepare_install
        calls = 0
        def prepare():
            nonlocal calls
            prepared = original()
            calls += 1
            if calls == 1:
                record = self.cluster.record()
                record["paused"] = {"owner": "competingstop", "drained": True,
                    "replicas": dict.fromkeys(WORKLOADS, 2), "hpas": []}
                self.cluster.save(record)
            return prepared
        self.cluster.events.clear()
        with patch.object(self.cluster, "prepare_install", side_effect=prepare):
            with self.assertRaisesRegex(ValueError, "Platform is stopped"):
                self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["paused"]["owner"], "competingstop")
        self.assertFalse(any(d["kind"] in {"Deployment", "Job"} for e in self.cluster.events if e[0] == "mutation" for d in e[1]))

    def test_cli_sigterm_records_interruption_and_releases_lock(self):
        import remote
        handlers = {}
        def terminate(*args):
            handlers[signal.SIGTERM](signal.SIGTERM, None)
        argv = ["remote.py", "install", "--kubeconfig", str(self.cluster.overlay / "config"),
                "--context", "test", "--overlay", str(self.cluster.overlay), "--image", self.cluster.image]
        with patch("remote.sys.argv", argv), patch("remote.signal.signal", side_effect=lambda sig, handler: handlers.update({sig: handler})), \
                patch("existing.ExistingCluster", return_value=self.cluster), patch.object(self.cluster, "ready", side_effect=terminate), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(remote.main(), 130)
        self.assertEqual(self.cluster.get("ConfigMap", NAME)["data"], {"owner": ""})
        self.assertEqual(self.cluster.record()["install"]["status"], "failed")

    def test_reserved_lock_cannot_enter_manifest_or_deletion_inventory(self):
        with self.assertRaisesRegex(ValueError, "reserved"):
            validate([resource("ConfigMap", NAME)])

    def test_nested_uninstall_stop_reuses_the_lock(self):
        self.output(self.cluster.install)
        with patch("existing.Maintenance"):
            self.output(self.cluster.uninstall)
        self.assertIsNone(self.cluster.get("ConfigMap", "hibana-platform"))
        self.assertEqual(self.cluster.get("ConfigMap", NAME)["data"], {"owner": ""})


def cp(name, state="running", *, ready=True, terminating=False):
    return {"metadata": {"name": name, "uid": name, **({"deletionTimestamp": "2026-09-10T00:00:00Z"} if terminating else {})},
            "status": {"phase": "Running" if state == "running" else "Pending", "containerStatuses": [
                {"name": "control-plane", "ready": ready, "restartCount": 0, "state": {state: {}}}]}}


class MaintenanceDrainTests(unittest.TestCase):
    def test_pending_and_crashloop_are_skipped_but_running_unready_and_terminating_are_checked(self):
        pods = {p["metadata"]["uid"]: p for p in [cp("healthy"), cp("starting", ready=False),
            cp("old", terminating=True), cp("pending", "waiting"), cp("crashed", "terminated")]}
        maintenance = Maintenance(MagicMock())
        with patch.object(maintenance, "pods", return_value=pods), patch.object(maintenance, "call",
                return_value='{"active_requests":0,"inflight_executions":0}') as call:
            maintenance.drain()
        self.assertEqual({c.args[0] for c in call.call_args_list}, {"healthy", "starting", "old"})

    def test_pending_container_start_without_uid_or_restart_change_requires_another_check(self):
        before = {"healthy": cp("healthy"), "starting": cp("starting", "waiting")}
        after = {**deepcopy(before), "starting": cp("starting", ready=False)}
        maintenance = Maintenance(MagicMock())
        with patch.object(maintenance, "pods", side_effect=[before, after, after, after]), patch.object(maintenance, "call",
                return_value='{"active_requests":0,"inflight_executions":0}') as call, patch("maintenance.time.sleep"):
            maintenance.drain()
        self.assertEqual([c.args[0] for c in call.call_args_list], ["healthy", "healthy", "starting"])

    def test_no_running_cp_cannot_certify_durable_executions_drained(self):
        maintenance = Maintenance(MagicMock())
        with patch.object(maintenance, "pods", return_value={"pending": cp("pending", "waiting")}), patch.object(maintenance, "call") as call:
            with self.assertRaisesRegex(ValueError, "execution status is unknown"):
                maintenance.drain()
        call.assert_not_called()

    def test_running_cp_exec_failure_is_not_treated_as_idle(self):
        maintenance = Maintenance(MagicMock())
        with patch.object(maintenance, "pods", return_value={"healthy": cp("healthy")}), patch.object(maintenance, "call",
                side_effect=subprocess.CalledProcessError(1, "kubectl")):
            with self.assertRaises(subprocess.CalledProcessError):
                maintenance.drain()


if __name__ == "__main__":
    unittest.main()
