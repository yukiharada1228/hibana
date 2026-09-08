"""Offline regression checks for local startup's data-preservation and ordering guarantees."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch

import kubernetes as k8s


class StartupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cluster = k8s.LocalCluster("hibana-dev")
        self.cluster.state = Path(self.temp.name)
        self.cluster.kubeconfig = self.cluster.state / "kubeconfig"

    def test_credentials_survive_repeated_startup_without_rotation(self):
        first = self.cluster.credentials()
        before = (self.cluster.state / "sdk.env").read_bytes()
        self.assertEqual(first, self.cluster.credentials())
        self.assertEqual(before, (self.cluster.state / "sdk.env").read_bytes())
        for name in ["sdk.env", "secrets.json"]:
            self.assertEqual((self.cluster.state / name).stat().st_mode & 0o777, 0o600)
        runtime = next(item for item in first["items"] if item["metadata"]["name"] == "hibana-runtime")
        self.assertEqual(set(runtime["stringData"]), {"DATABASE_URL"})

    def test_partial_credentials_are_not_replaced(self):
        target = self.cluster.state / "secrets.json"
        target.write_text('{"items": []}')
        with self.assertRaisesRegex(ValueError, "Incomplete credentials"):
            self.cluster.credentials()
        self.assertEqual(target.read_text(), '{"items": []}')
        self.assertFalse((self.cluster.state / "sdk.env").exists())

    def test_existing_tenant_accepts_session_creation_status(self):
        self.cluster.credentials()
        with patch("kubernetes.urllib.request.urlopen") as request:
            request.return_value.__enter__.return_value.status = 201
            self.cluster.bootstrap()
        self.assertEqual(request.call_count, 1, "an existing tenant must not be recreated")
        self.assertTrue(request.call_args.args[0].full_url.endswith("/auth/login"))

    def test_new_tenant_is_verified_after_bootstrap(self):
        self.cluster.credentials()
        created = MagicMock()
        created.__enter__.return_value.status = 201
        unauthenticated = k8s.urllib.error.HTTPError("", 401, "", {}, None)
        with patch("kubernetes.urllib.request.urlopen", side_effect=[unauthenticated, created, created]) as request:
            self.cluster.bootstrap()
        self.assertEqual([call.args[0].full_url.split("18080")[-1] for call in request.call_args_list],
                         ["/auth/login", "/admin/tenants", "/auth/login"])

    def test_old_cluster_is_rejected_without_mutation(self):
        self.cluster.kubeconfig.touch()
        with patch("kubernetes.shutil.which", return_value="tool"), patch("kubernetes.run", side_effect=["hibana-dev\n", "{}"]) as command:
            with self.assertRaisesRegex(ValueError, "left unchanged"):
                self.cluster.ensure_cluster()
        self.assertEqual([call.args[1] for call in command.call_args_list], ["get", "inspect"])

    def test_unowned_cluster_is_rejected_before_access(self):
        with patch("kubernetes.shutil.which", return_value="tool"), patch("kubernetes.run", return_value="hibana-dev\n") as command:
            with self.assertRaisesRegex(ValueError, "checkout-owned"):
                self.cluster.ensure_cluster()
        self.assertEqual(command.call_count, 1)

    def test_failed_or_running_migration_is_not_deleted(self):
        for status in [{"active": 1}, {"failed": 1}]:
            with self.subTest(status=status), patch.object(self.cluster, "kube", return_value=json.dumps({"status": status})) as command:
                with self.assertRaisesRegex(ValueError, "not deleted"):
                    self.cluster.migrate("hibana-platform:test")
                self.assertEqual(command.call_count, 1)

    def test_failed_migration_prevents_application_deployment(self):
        resources = [{"kind": "Namespace"}, {"kind": "Deployment"}]
        with patch.object(self.cluster, "ensure_cluster"), \
                patch.object(self.cluster, "credentials", return_value={"items": []}), \
                patch("kubernetes.run", return_value="sha256:" + "a" * 64), \
                patch.object(self.cluster, "render", return_value=resources), \
                patch("kubernetes.stamp_runtime_settings"), \
                patch.object(self.cluster, "kube"), \
                patch.object(self.cluster, "apply") as apply, \
                patch.object(self.cluster, "dependency_path", return_value=k8s.LOCAL / "dependencies"), \
                patch.object(self.cluster, "migrate", side_effect=subprocess.CalledProcessError(1, "migration")), \
                patch.object(self.cluster, "bootstrap") as bootstrap:
            with self.assertRaises(subprocess.CalledProcessError):
                self.cluster.install()
        self.assertFalse(any(doc["kind"] == "Deployment" for call in apply.call_args_list for doc in call.args[0]))
        bootstrap.assert_not_called()

    def test_image_replacement_only_changes_platform_container(self):
        rendered = """kind: Deployment
spec:
  template:
    spec:
      containers:
        - {name: platform, image: 'hibana-platform:dev'}
        - {name: dependency, image: 'postgres:17'}
"""
        with patch("kubernetes.run", return_value=rendered):
            docs = self.cluster.render("local", "hibana-platform:local-content")
        self.assertEqual([c["image"] for c in docs[0]["spec"]["template"]["spec"]["containers"]],
                         ["hibana-platform:local-content", "postgres:17"])

    def test_config_changes_trigger_rollout_but_unused_credentials_do_not(self):
        config = {"kind": "ConfigMap", "metadata": {"name": "config"}, "data": {"limit": "1"}}
        deployment = {"kind": "Deployment", "spec": {"template": {"spec": {"containers": [
            {"envFrom": [{"configMapRef": {"name": "config"}}]},
        ]}}}}
        docs = [config, deployment]
        k8s.stamp_runtime_settings(docs, [])
        annotations = deployment["spec"]["template"]["metadata"]["annotations"]
        first = annotations.copy()
        k8s.stamp_runtime_settings(docs, [{"kind": "Secret", "metadata": {"name": "unused"}, "stringData": {"key": "unused"}}])
        self.assertEqual(first, annotations)
        config["data"]["limit"] = "2"
        k8s.stamp_runtime_settings(docs, [])
        self.assertNotEqual(first, annotations)


class LifecycleTests(unittest.TestCase):
    def test_local_stop_targets_only_owned_running_nodes(self):
        cluster = k8s.LocalCluster("hibana-check")
        nodes = [{"Id": "owned-a", "State": {"Running": True}}, {"Id": "owned-b", "State": {"Running": False}}]
        stopped = [{"Id": n["Id"], "State": {"Running": False}} for n in nodes]
        with patch.object(cluster, "owned_nodes", side_effect=[nodes, stopped]), patch("kubernetes.run") as command:
            cluster.stop()
        command.assert_called_once_with("docker", "stop", "--timeout", "60", "owned-a")

    def test_local_start_waits_for_live_api_before_ready_checks(self):
        cluster = k8s.LocalCluster("hibana-check")
        with patch.object(cluster, "owned_nodes", return_value=[{"Id": "owned", "Name": "/hibana-check-control-plane", "State": {"Running": False}}]), patch("kubernetes.run") as command, patch.object(cluster, "kube") as kube, patch.object(cluster, "wait_workloads"):
            cluster.start()
        command.assert_called_once_with("docker", "start", "owned")
        self.assertIn("/readyz", kube.call_args_list[0].args)
        self.assertEqual(sum("rollout" in call.args for call in kube.call_args_list), 2)

    def test_persisted_ready_state_cannot_complete_restart(self):
        from datetime import datetime, timezone
        resumed = datetime(2026, 9, 8, 1, tzinfo=timezone.utc)
        pod = {"metadata": {"labels": {"app.kubernetes.io/name": "hibana-worker"}}, "status": {"containerStatuses": [{"ready": True, "state": {"running": {"startedAt": "2026-09-07T01:00:00Z"}}}]}}
        self.assertFalse(k8s.workloads_ready([pod], {"hibana-worker": 1}, resumed))
        pod["status"]["containerStatuses"][0]["state"]["running"]["startedAt"] = "2026-09-08T01:00:05Z"
        self.assertTrue(k8s.workloads_ready([pod], {"hibana-worker": 1}, resumed))
        self.assertFalse(k8s.workloads_ready([pod], {"hibana-worker": 2}, resumed))

    def test_new_cluster_has_no_existing_dependency_json(self):
        with patch.object(k8s.LocalCluster, "kube", return_value=""):
            self.assertEqual(k8s.LocalCluster().dependency_path(), k8s.LOCAL.parent / "persistent-dependencies")

    def test_uninstall_failure_retains_credentials(self):
        cluster = k8s.LocalCluster("hibana-check")
        with tempfile.TemporaryDirectory() as directory:
            cluster.state = Path(directory)
            secret = cluster.state / "secrets.json"
            secret.write_text("retained")
            with patch.object(cluster, "owned_nodes"), patch("kubernetes.run", side_effect=["", "leftover"]):
                with self.assertRaisesRegex(ValueError, "incomplete"):
                    cluster.uninstall()
            self.assertEqual(secret.read_text(), "retained")

    def test_existing_overlay_rejects_foreign_namespace_and_cluster_resources(self):
        from existing import validate
        for kind, name, namespace in [("Namespace", "other", None), ("ClusterRole", "hibana-role", None), ("Secret", "hibana-secret", "default")]:
            with self.assertRaises(ValueError):
                validate([{"kind": kind, "metadata": {"name": name, "namespace": namespace}}])

    def test_existing_uninstall_keeps_pvc_and_refuses_changed_ownership(self):
        from existing import ExistingCluster, LABEL
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit-context")
            record = {"resources": [{"kind": "PersistentVolumeClaim", "name": "hibana-data"}, {"kind": "Deployment", "name": "hibana-worker"}]}
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "get", return_value={"metadata": {"labels": {LABEL: "another"}}}), patch.object(cluster, "kube") as kube:
                with self.assertRaisesRegex(ValueError, "ownership changed"):
                    cluster.uninstall()
                kube.assert_not_called()
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "get", return_value={"metadata": {"labels": {LABEL: "hibana"}}}), patch.object(cluster, "kube") as kube:
                cluster.uninstall()
                self.assertFalse(any("PersistentVolumeClaim" in call.args for call in kube.call_args_list))

    def test_existing_stop_refuses_foreign_hpa_before_any_mutation(self):
        from existing import ExistingCluster, LABEL
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit-context")
            deployment = {"metadata": {"labels": {LABEL: "hibana"}}, "spec": {"replicas": 2}}
            hpa = {"metadata": {"labels": {}}, "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "hibana-worker"}}}
            with patch.object(cluster, "record", return_value={"resources": []}), patch.object(cluster, "get", return_value=deployment), patch.object(cluster, "kube", return_value=json.dumps({"items": [hpa]})) as kube, patch.object(cluster, "save") as save:
                with self.assertRaisesRegex(ValueError, "external HPA"):
                    cluster.stop()
                save.assert_not_called()
                self.assertEqual(kube.call_count, 1)

    def test_existing_inventory_cannot_expand_to_cluster_resources(self):
        from existing import ExistingCluster, LABEL
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit-context")
            for kind in ["Namespace", "ClusterRole"]:
                marker = {"metadata": {"labels": {LABEL: "hibana"}}, "data": {"state.json": json.dumps({"resources": [{"kind": kind, "name": "hibana"}]})}}
                with patch.object(cluster, "get", return_value=marker):
                    with self.assertRaises(ValueError):
                        cluster.record()

    def test_existing_stop_start_restores_replicas_and_owned_hpa(self):
        from existing import ExistingCluster, LABEL
        from copy import deepcopy
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit-context")
            record = {"resources": []}
            hpa = {"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "hibana-worker", "namespace": "hibana", "labels": {LABEL: "hibana"}}, "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "hibana-worker"}, "minReplicas": 2, "maxReplicas": 8}}
            def deployment(kind, name):
                return {"metadata": {"labels": {LABEL: "hibana"}}, "spec": {"replicas": 4 if name.endswith("worker") else 2}}
            events = []
            def kube(*args, **kwargs):
                events.append(args)
                return json.dumps({"items": [hpa]}) if "get" in args else ""
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "get", side_effect=deployment), patch.object(cluster, "kube", side_effect=kube), patch.object(cluster, "save", side_effect=lambda r: events.append(("save", deepcopy(r)))) as save, patch.object(cluster, "ready"), patch.object(cluster, "apply") as apply:
                cluster.stop()
                self.assertEqual(events[1][0], "save", "resume settings must be saved before scaling")
                self.assertEqual(record["paused"]["replicas"], {"hibana-control-plane": 2, "hibana-worker": 4})
                cluster.start()
                self.assertNotIn("paused", record)
                apply.assert_called_once_with([hpa])
                self.assertIn(("-n", "hibana", "scale", "deployment/hibana-worker", "--replicas=4"), events)


if __name__ == "__main__":
    unittest.main()
