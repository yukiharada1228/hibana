"""Probe real operator sequencing with a deterministic Pod API and clock."""
from copy import deepcopy
import contextlib
import io
import json
import subprocess
import unittest
from unittest.mock import MagicMock, patch

from readiness import Readiness


def pod(name, component="worker", *, terminating=False):
    return {"metadata": {"name": name, "uid": name + "-uid", **({"deletionTimestamp": "2026-09-10T00:00:00Z"} if terminating else {})},
            "spec": {"terminationGracePeriodSeconds": 240},
            "status": {"phase": "Running", "podIP": "10.0.0." + str(len(name)), "containerStatuses": [{
                "name": component, "ready": True, "restartCount": 0, "containerID": name + "-container", "state": {"running": {}}}]}}


class ReadinessTests(unittest.TestCase):
    def setUp(self):
        self.now = 0
        self.target = MagicMock()
        self.checks = Readiness(self.target)
        self.fleet = {"control-plane": [pod("cp-one", "control-plane"), pod("cp-two", "control-plane")],
                      "worker": [pod("worker-one"), pod("worker-two")]}
        self.bad = None
        self.probes = []
        def kube(*args, **kwargs):
            if "exec" in args:
                component = args[args.index("check") + 1]
                self.probes.append(args)
                keys = {"database", "object_store", "workers", "redis"} if component == "control-plane" else {
                    "database", "object_store", "control_planes"}
                return json.dumps({"protocol": 1, "checks": {k: (component, k) != self.bad for k in keys}})
            selector = args[args.index("-l") + 1]
            component = "control-plane" if selector.endswith("control-plane") else "worker"
            return json.dumps({"items": deepcopy(self.fleet[component])})
        self.target.kube.side_effect = kube
        for mock in (patch("readiness.time.monotonic", side_effect=lambda: self.now),
                     patch("readiness.time.sleep", side_effect=self.advance)):
            mock.start()
            self.addCleanup(mock.stop)

    def advance(self, seconds):
        self.now += seconds

    def verify(self, **kwargs):
        with contextlib.redirect_stdout(io.StringIO()):
            self.checks.verify_dependencies(timeout=8, **kwargs)

    def test_fresh_probes_cover_every_replica_and_remain_healthy(self):
        self.verify()
        self.assertEqual({a[a.index("-c") + 1] for a in self.probes}, {"control-plane", "worker"})
        self.assertEqual({a[a.index("exec") + 1] for a in self.probes}, {"pod/" + p["metadata"]["name"] for ps in self.fleet.values() for p in ps})
        self.assertGreaterEqual(self.now, 5)
        self.assertGreater(len(self.probes), 4)
        for call in self.target.kube.call_args_list:
            if "exec" in call.args:
                self.assertTrue(call.kwargs["quiet"])
                self.assertLessEqual(call.kwargs["timeout"], 8)

    def test_unhealthy_worker_database_is_not_hidden_by_healthy_control_plane(self):
        self.bad = ("worker", "database")
        with self.assertRaisesRegex(ValueError, "worker-one: database"):
            self.verify()

    def test_absent_or_terminating_fleet_never_succeeds(self):
        self.fleet["worker"] = []
        with self.assertRaisesRegex(ValueError, "worker Pods are not ready"):
            self.verify(stable_seconds=0)
        self.now = 0
        self.fleet["worker"] = [pod("worker-one", terminating=True)]
        with self.assertRaisesRegex(ValueError, "worker Pods are not ready"):
            self.verify(stable_seconds=0)

    def test_pod_change_during_probe_invalidates_the_successful_result(self):
        original = self.target.kube.side_effect
        def kube(*args, **kwargs):
            output = original(*args, **kwargs)
            if "exec" in args:
                self.fleet["worker"][0]["metadata"]["deletionTimestamp"] = "now"
            return output
        self.target.kube.side_effect = kube
        with self.assertRaisesRegex(ValueError, "worker Pods are not ready"):
            self.verify(stable_seconds=0)

    def test_bad_protocol_missing_check_and_exec_error_fail_without_leaking_output(self):
        original = self.target.kube.side_effect
        for response in ('private-credential', '{"protocol":1,"checks":{}}', subprocess.CalledProcessError(1, "kubectl", stderr="private-credential")):
            self.now = 0
            def kube(*args, **kwargs):
                if "exec" in args:
                    if isinstance(response, Exception):
                        raise response
                    return response
                return original(*args, **kwargs)
            self.target.kube.side_effect = kube
            with self.subTest(response=type(response).__name__), self.assertRaisesRegex(ValueError, "Dependency verification failed") as caught:
                self.verify()
            self.assertNotIn("private-credential", str(caught.exception))

    def test_waits_for_terminating_pod_uid_but_keeps_unchanged_and_completed_pods(self):
        old, unchanged, complete = pod("old", terminating=True), pod("unchanged"), pod("complete", terminating=True)
        complete["status"]["phase"] = "Succeeded"
        self.target.kube.side_effect = [json.dumps({"items": [old, unchanged, complete]}), json.dumps({"items": [unchanged, complete]})]
        self.checks.wait_termination()
        self.assertEqual(self.now, 0.5)

    def test_termination_timeout_honors_pod_grace_and_preserves_access(self):
        old = pod("old", terminating=True)
        self.target.kube.side_effect = lambda *args, **kwargs: json.dumps({"items": [old]})
        with self.assertRaisesRegex(ValueError, "Network access was retained"):
            self.checks.wait_termination()
        self.assertEqual(self.now, 270)
        self.assertTrue(all("get" in c.args for c in self.target.kube.call_args_list))


if __name__ == "__main__":
    unittest.main()
