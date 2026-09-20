"""Offline regression checks for local startup's data-preservation and ordering guarantees."""
import json
from contextlib import nullcontext
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch

import kubernetes as k8s
from test_preflight import PreflightTests, RemoteInstallTests
from test_operation import OperationTests, MaintenanceDrainTests, cp
from test_readiness import ReadinessTests
from test_local_operation import LocalOperationTests
from maintenance import Maintenance


class StartupTests(unittest.TestCase):
    def setUp(self):
        env = patch.dict(k8s.os.environ, {
            "OIDC_ISSUER_URL": "https://id.test", "OIDC_CLIENT_ID": "hibana", "OIDC_CLIENT_SECRET": " fixture-secret ",
            "OIDC_CALLBACK_URL": "https://console.test/api/auth/oidc/callback", "OIDC_CONSOLE_URL": "https://console.test/",
            "HIBANA_ADMIN_OIDC_SUBJECT": "fixture-admin",
            "HIBANA_OIDC_EGRESS_CIDRS": "192.0.2.10/32",
            "HIBANA_OIDC_EGRESS_PORTS": "443",
        })
        env.start()
        self.addCleanup(env.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cluster = k8s.LocalCluster("hibana-dev")
        self.cluster.state = Path(self.temp.name)
        self.cluster.kubeconfig = self.cluster.state / "kubeconfig"

    def test_credentials_survive_repeated_startup_without_rotation(self):
        first = self.cluster.credentials()
        control = next(item for item in first["items"] if item["metadata"]["name"] == "hibana-control-plane")
        self.assertEqual(control["stringData"]["OIDC_CLIENT_SECRET"], " fixture-secret ")
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

    def test_existing_tenant_does_not_change_identity_bindings(self):
        self.cluster.credentials()
        conflict = k8s.urllib.error.HTTPError("", 409, "", {}, None)
        with patch("kubernetes.urllib.request.urlopen", side_effect=conflict) as request:
            self.cluster.bootstrap()
        self.assertEqual(request.call_count, 1)
        self.assertTrue(request.call_args.args[0].full_url.endswith("/admin/tenants"))
        body = json.loads(request.call_args.args[0].data)
        self.assertEqual(body["admin_oidc_subject"], "fixture-admin")
        self.assertNotIn("admin_password", body)

    def test_new_tenant_uses_external_subject_without_password_login(self):
        self.cluster.credentials()
        with patch("kubernetes.urllib.request.urlopen") as request:
            request.return_value.__enter__.return_value.status = 201
            self.cluster.bootstrap()
        self.assertEqual(request.call_count, 1)
        self.assertTrue(request.call_args.args[0].full_url.endswith("/admin/tenants"))

    def test_invalid_admin_identity_is_rejected_before_mutation(self):
        for key, value in [
            ("HIBANA_ADMIN_EMAIL", " \t"),
            ("HIBANA_ADMIN_OIDC_SUBJECT", "x" * 256),
            ("HIBANA_ADMIN_OIDC_SUBJECT", "é" * 128),
            ("HIBANA_ADMIN_OIDC_SUBJECT", "subject\nvalue"),
            ("HIBANA_ADMIN_OIDC_SUBJECT", "subject\x85value"),
        ]:
            for action in (lambda: self.cluster.preview("install"), self.cluster.install, self.cluster.credentials):
                with self.subTest(key=key, action=action), patch.dict(k8s.os.environ, {key: value}), \
                        patch("kubernetes.run") as run, patch.object(self.cluster, "ensure_cluster") as ensure:
                    with self.assertRaisesRegex(ValueError, key):
                        action()
                    run.assert_not_called()
                    ensure.assert_not_called()
                    self.assertEqual(list(self.cluster.state.iterdir()), [])

    def test_subject_is_preserved_and_empty_subject_skips_bootstrap(self):
        subject = "é" * 127 + " "
        with patch.dict(k8s.os.environ, {"HIBANA_ADMIN_OIDC_SUBJECT": subject}):
            self.cluster.credentials()
        self.assertEqual(self.cluster.sdk_env()["HIBANA_ADMIN_OIDC_SUBJECT"], subject)
        path = self.cluster.state / "sdk.env"
        path.write_text("\n".join("HIBANA_ADMIN_OIDC_SUBJECT=''" if line.startswith("HIBANA_ADMIN_OIDC_SUBJECT=") else line
                                  for line in path.read_text().splitlines()) + "\n")
        self.cluster.credential_settings()
        with patch("kubernetes.urllib.request.urlopen") as request:
            self.cluster.bootstrap()
        request.assert_not_called()

    def test_saved_credentials_require_current_fields_before_cluster_changes(self):
        self.cluster.credentials()
        path = self.cluster.state / "sdk.env"
        original = path.read_text()
        for key in ("HIBANA_URL", "HIBANA_TENANT", "BOOTSTRAP_ADMIN_TOKEN", "HIBANA_ADMIN_EMAIL", "HIBANA_ADMIN_OIDC_SUBJECT"):
            path.write_text("\n".join(line for line in original.splitlines() if not line.startswith(key + "=")) + "\n")
            path.chmod(0o400)
            before = {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()}
            for action in (lambda: self.cluster.preview("install"), self.cluster.install, self.cluster.credentials):
                with self.subTest(key=key, action=action), patch.dict(k8s.os.environ, {key: "must-not-hide-missing-setting"}), \
                        patch("kubernetes.run") as run, patch.object(self.cluster, "ensure_cluster") as ensure:
                    with self.assertRaisesRegex(ValueError, "sdk.env.*" + key):
                        action()
                    run.assert_not_called()
                    ensure.assert_not_called()
                    self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()})
            path.chmod(0o600)

    def test_invalid_saved_syntax_has_a_redacted_actionable_error(self):
        self.cluster.credentials()
        path = self.cluster.state / "sdk.env"
        path.write_text("BOOTSTRAP_ADMIN_TOKEN='private-fixture-secret")
        with self.assertRaisesRegex(ValueError, "KEY=value.*sdk.env") as error:
            self.cluster.credential_settings()
        self.assertNotIn("private-fixture-secret", str(error.exception))

    def test_missing_oidc_does_not_create_local_credentials(self):
        with patch.dict(k8s.os.environ, {"OIDC_ISSUER_URL": ""}):
            with self.assertRaisesRegex(ValueError, "OIDC_ISSUER_URL"):
                self.cluster.credentials()
        self.assertFalse((self.cluster.state / "secrets.json").exists())

    def test_invalid_oidc_is_rejected_before_preview_install_or_saving(self):
        cases = [
            ("OIDC_CLIENT_SECRET", " "),
            ("OIDC_ISSUER_URL", "http://id.test"),
            ("OIDC_CALLBACK_URL", "https://console.test:invalid/callback"),
            ("OIDC_CONSOLE_URL", "https://user:private-secret@console.test/"),
            ("OIDC_CONSOLE_URL", "https://console.test/?"),
            ("OIDC_CONSOLE_URL", "https://console.test/#"),
            ("OIDC_ISSUER_URL", "https://id.test\n/realm"),
            ("OIDC_ALLOW_INSECURE_HTTP", "yes"),
            ("OIDC_SESSION_TTL_SECS", "not-a-number"),
            ("OIDC_SESSION_TTL_SECS", "3_600"),
            ("OIDC_SESSION_TTL_SECS", "59"),
            ("OIDC_SESSION_TTL_SECS", "3601"),
        ]
        for key, value in cases:
            for action in (lambda: self.cluster.preview("install"), self.cluster.install, self.cluster.credentials):
                with self.subTest(key=key, action=action), patch.dict(k8s.os.environ, {key: value}), \
                        patch("kubernetes.run") as run, patch.object(self.cluster, "ensure_cluster") as ensure:
                    with self.assertRaisesRegex(ValueError, key) as error:
                        action()
                    self.assertNotIn("private-secret", str(error.exception))
                    run.assert_not_called()
                    ensure.assert_not_called()
                    self.assertEqual(list(self.cluster.state.iterdir()), [])

    def test_loopback_http_requires_explicit_development_setting(self):
        for url in ("http://localhost:8180/realm", "http://127.0.0.1:8180/realm", "http://[::1]:8180/realm"):
            with self.subTest(url=url), patch.dict(k8s.os.environ, {"OIDC_ISSUER_URL": url, "OIDC_ALLOW_INSECURE_HTTP": "false"}):
                with self.assertRaisesRegex(ValueError, "OIDC_ISSUER_URL"):
                    self.cluster.credential_settings()
                with patch.dict(k8s.os.environ, {"OIDC_ALLOW_INSECURE_HTTP": "true"}):
                    self.cluster.credential_settings()
        with patch.dict(k8s.os.environ, {"OIDC_ISSUER_URL": "http://id.test", "OIDC_ALLOW_INSECURE_HTTP": "true"}):
            with self.assertRaisesRegex(ValueError, "OIDC_ISSUER_URL"):
                self.cluster.credentials()

    def test_saved_oidc_is_validated_without_overwrite_or_permission_changes(self):
        stored = self.cluster.credentials()
        control = next(item["stringData"] for item in stored["items"] if item["metadata"]["name"] == "hibana-control-plane")
        control["OIDC_SESSION_TTL_SECS"] = "invalid"
        path = self.cluster.state / "secrets.json"
        path.write_text(json.dumps(stored))
        path.chmod(0o400)
        before = {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()}
        for action in (lambda: self.cluster.preview("install"), self.cluster.install, self.cluster.credentials):
            with patch.object(self.cluster, "ensure_cluster") as ensure:
                with self.assertRaisesRegex(ValueError, "Correct OIDC settings.*secrets.json"):
                    action()
                ensure.assert_not_called()
                self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()})

    def test_preview_uses_saved_oidc_and_does_not_chmod_credentials(self):
        self.cluster.credentials()
        for path in self.cluster.state.iterdir():
            path.chmod(0o400)
        before = {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()}
        with patch.dict(k8s.os.environ, {"OIDC_ISSUER_URL": ""}), \
                patch("kubernetes.shutil.which", return_value="tool"), patch.object(self.cluster, "render", return_value=[]), \
                patch("kubernetes.run", return_value=""):
            self.cluster.preview("install")
        self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mode) for p in self.cluster.state.iterdir()})

    def test_old_cluster_is_rejected_without_mutation(self):
        self.cluster.kubeconfig.touch()
        with patch("kubernetes.shutil.which", return_value="tool"), patch("kubernetes.run", side_effect=["hibana-dev\n", "{}"]) as command:
            with self.assertRaisesRegex(ValueError, "left unchanged"):
                self.cluster.ensure_cluster()
        self.assertEqual([call.args[1] for call in command.call_args_list], ["get", "inspect"])

    def test_invalid_oidc_egress_is_rejected_before_cluster_or_credentials(self):
        for key, value in [("HIBANA_OIDC_EGRESS_CIDRS", cidr) for cidr in
                           ("", "192.0.2.10", "192.0.2.10/24", "0.0.0.0/0", "::/0", "id.example/32")] + [
                           ("HIBANA_OIDC_EGRESS_PORTS", port) for port in ("", "0", "65536", "https", "443,")]:
            for action in (lambda: self.cluster.preview("install"), self.cluster.install):
                with self.subTest(key=key, value=value), patch.dict(k8s.os.environ, {key: value}), \
                        patch.object(self.cluster, "ensure_cluster") as ensure, patch("kubernetes.run") as run:
                    with self.assertRaisesRegex(ValueError, "HIBANA_OIDC_EGRESS"):
                        action()
                    ensure.assert_not_called()
                    run.assert_not_called()
                    self.assertEqual(list(self.cluster.state.iterdir()), [])

    def test_saved_egress_is_reused_and_preview_does_not_write_overrides(self):
        self.cluster.credentials()
        path = self.cluster.state / "oidc-egress.json"
        path.write_text(json.dumps({"cidrs": ["192.0.2.20/32", "2001:db8::20/128"], "ports": [8443]}))
        path.chmod(0o400)
        before = path.read_bytes(), path.stat().st_mode
        with patch.dict(k8s.os.environ, {}, clear=True), patch("kubernetes.shutil.which", return_value="tool"), \
                patch.object(self.cluster, "render", return_value=[]), patch("kubernetes.run", return_value=""):
            settings, _ = self.cluster.oidc_egress()
            self.assertEqual(settings["ports"], [8443])
            self.assertEqual(settings["cidrs"], ["192.0.2.20/32", "2001:db8::20/128"])
            self.cluster.preview("install")
            with patch.dict(k8s.os.environ, {"HIBANA_OIDC_EGRESS_CIDRS": "192.0.2.30/32"}):
                self.cluster.preview("install")
                self.assertEqual(self.cluster.oidc_egress()[0], {"cidrs": ["192.0.2.30/32"], "ports": [8443]})
        self.assertEqual(before, (path.read_bytes(), path.stat().st_mode))

    def test_invalid_saved_egress_is_rejected_without_replacement(self):
        path = self.cluster.state / "oidc-egress.json"
        for invalid in ("{", "[]", '{"cidrs":["192.0.2.10/32"],"ports":[true]}'):
            path.write_text(invalid)
            with patch.dict(k8s.os.environ, {}, clear=True):
                with self.assertRaisesRegex(ValueError, "oidc-egress.json"):
                    self.cluster.oidc_egress()
            self.assertEqual(path.read_text(), invalid)

    def test_install_applies_and_saves_control_plane_egress(self):
        from common import KubernetesTarget
        docs = KubernetesTarget().render(k8s.LOCAL, "hibana-platform:fixture")
        with patch.object(self.cluster, "preflight"), patch.object(self.cluster, "ensure_cluster"), \
                patch("kubernetes.run", return_value="sha256:" + "a" * 64), \
                patch.object(self.cluster, "render", return_value=docs), patch.object(self.cluster, "kube"), \
                patch.object(self.cluster, "apply") as apply, patch.object(self.cluster, "migrate"), \
                patch.object(self.cluster, "dependency_path", return_value=k8s.LOCAL / "dependencies"), \
                patch.object(self.cluster, "resume_admission"), patch("kubernetes.wait_http"), \
                patch.object(self.cluster, "bootstrap"):
            self.cluster.install()
        policies = [doc for call in apply.call_args_list for doc in call.args[0] if doc["kind"] == "NetworkPolicy"]
        policy = next(p for p in policies if p["metadata"]["name"] == "hibana-local-identity-provider")
        self.assertEqual(policy["spec"]["podSelector"], {"matchLabels": {"app.kubernetes.io/name": "hibana-control-plane"}})
        self.assertEqual(policy["spec"]["egress"], [{"to": [{"ipBlock": {"cidr": "192.0.2.10/32"}}], "ports": [{"protocol": "TCP", "port": 443}]}])
        self.assertIn("default-deny", {p["metadata"]["name"] for p in policies})
        path = self.cluster.state / "oidc-egress.json"
        self.assertEqual(json.loads(path.read_text()), {"cidrs": ["192.0.2.10/32"], "ports": [443]})
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)

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
        with patch.object(self.cluster, "preflight"), patch.object(self.cluster, "ensure_cluster"), \
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

    def test_local_install_preview_checks_tools_and_does_not_create_cluster_or_build(self):
        with patch("kubernetes.shutil.which", return_value="tool"), patch.object(self.cluster, "render", return_value=[]), \
                patch("kubernetes.run", return_value="") as run:
            self.cluster.preview("install")
        self.assertTrue(any(call.args[1] == "info" for call in run.call_args_list))
        self.assertFalse(any(action in call.args for call in run.call_args_list for action in ["create", "build", "apply", "delete"]))

    def test_image_replacement_only_changes_platform_container(self):
        for old in ("hibana-platform:dev", "registry.example.com/hibana/platform:replace-with-release",
                    "registry.test/hibana:v1", "registry.test/hibana@sha256:" + "a" * 64):
            with self.subTest(old=old):
                docs = []
                for kind, name in (("Deployment", "control-plane"), ("Deployment", "worker"),
                                   ("Job", "migrate"), ("Deployment", "console"), ("Job", "setup")):
                    docs.append({"kind": kind, "metadata": {"name": "hibana-" + name}, "spec": {"template": {"spec": {
                        "containers": [{"name": name, "image": old}, {"name": "dependency", "image": old}],
                        "initContainers": [{"name": "init", "image": old}]}}}})
                rendered = "\n---\n".join(json.dumps(doc) for doc in docs)
                with patch("kubernetes.run", return_value=rendered):
                    updated = self.cluster.render("local", "hibana-platform:local-content")
                for index, doc in enumerate(updated):
                    pod = doc["spec"]["template"]["spec"]
                    self.assertEqual([c["image"] for c in pod["containers"]],
                                     ["hibana-platform:local-content" if index < 3 else old, old])
                    self.assertEqual(pod["initContainers"][0]["image"], old)

    def test_missing_or_duplicate_platform_container_is_rejected(self):
        for containers in ([], [{"name": "renamed", "image": "old"}],
                           [{"name": "worker", "image": "old"}] * 2):
            with self.subTest(containers=containers):
                doc = {"kind": "Deployment", "metadata": {"name": "hibana-worker"},
                       "spec": {"template": {"spec": {"containers": containers}}}}
                with patch("kubernetes.run", return_value=json.dumps(doc)):
                    with self.assertRaisesRegex(ValueError, "hibana-worker must contain exactly one worker container"):
                        self.cluster.render("local", "hibana-platform:new")

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


class MaintenanceCompatibilityTests(unittest.TestCase):
    def test_probe_reads_binary_without_executing_legacy_server(self):
        target = MagicMock()
        maintenance = Maintenance(target)
        pods = {"cp": cp("cp")}
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "control-plane"
            def exec_probe(*args, **kwargs):
                command = list(args[args.index("--") + 1:])
                self.assertEqual(command[:2], ["sh", "-c"])
                self.assertEqual(command[-1], "/usr/local/bin/hibana-control-plane")
                command[-1] = str(binary)
                return subprocess.run(command, capture_output=True, text=True, check=True).stdout
            target.kube.side_effect = exec_probe
            # Neither fixture can be executed: the probe must inspect bytes only.
            with patch.object(maintenance, "pods", return_value=pods):
                binary.write_bytes(b"\x7fELF\0original server\0")
                self.assertFalse(maintenance.supports_protocol())
                binary.write_bytes(b"\x7fELF\0Usage: --maintenance close|open OWNER | status | prepare OWNER WORKER_IPS\0")
                self.assertTrue(maintenance.supports_protocol())
                binary.write_bytes(b"\x7fELF\0Usage: --maintenance different-protocol\0")
                with self.assertRaisesRegex(ValueError, "Could not determine"):
                    maintenance.supports_protocol()
                binary.unlink()
                with self.assertRaises(subprocess.CalledProcessError):
                    maintenance.supports_protocol()

    def test_probe_errors_are_never_treated_as_legacy(self):
        for error in [subprocess.CalledProcessError(1, "kubectl"), subprocess.TimeoutExpired("kubectl", 15)]:
            with self.subTest(error=type(error).__name__):
                target = MagicMock()
                target.kube.side_effect = error
                maintenance = Maintenance(target)
                with patch.object(maintenance, "pods", return_value={"cp": cp("cp")}):
                    with self.assertRaises(type(error)):
                        maintenance.supports_protocol()
        maintenance = Maintenance(MagicMock())
        maintenance.target.kube.return_value = ""
        with patch.object(maintenance, "pods", return_value={"cp": cp("cp")}):
            with self.assertRaisesRegex(ValueError, "Could not determine"):
                maintenance.supports_protocol()

    def test_mixed_or_replaced_control_planes_cannot_select_legacy_mode(self):
        target = MagicMock()
        maintenance = Maintenance(target)
        pods = {"old": cp("old"), "new": cp("new")}
        target.kube.side_effect = ["legacy", "supported"]
        with patch.object(maintenance, "pods", return_value=pods):
            with self.assertRaisesRegex(ValueError, "mixes maintenance protocols"):
                maintenance.supports_protocol()
        target.kube.side_effect = None
        target.kube.return_value = "legacy"
        with patch.object(maintenance, "pods", side_effect=[{"old": cp("old")}, {"new": cp("new")}]):
            with self.assertRaisesRegex(ValueError, "Control Plane changed"):
                maintenance.supports_protocol()


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        local_root = tempfile.TemporaryDirectory()
        self.addCleanup(local_root.cleanup)
        root = patch("kubernetes.ROOT", Path(local_root.name))
        root.start()
        self.addCleanup(root.stop)
        # These tests isolate lifecycle sequencing. Real acquisition, competing
        # operators and nested uninstall/stop are covered by OperationTests.
        lock = patch("existing.ExistingCluster.operation", side_effect=lambda action: nullcontext())
        lock.start()
        self.addCleanup(lock.stop)

    def test_operator_selects_ready_container_and_skips_crashloop(self):
        from maintenance import Maintenance, NoControlPlane
        maintenance = Maintenance(MagicMock())
        pods = {name: {"metadata": {"name": name}, "status": {"phase": "Running", "containerStatuses": [
            {"name": "control-plane", "ready": ready, "state": state}]}} for name, ready, state in [
                ("a-crashloop", False, {"waiting": {"reason": "CrashLoopBackOff"}}),
                ("b-starting", False, {"running": {}}), ("c-ready", True, {"running": {}})]}
        with patch.object(maintenance, "pods", return_value=pods):
            self.assertEqual(maintenance.control_plane(), "c-ready")
            del pods["c-ready"]
            self.assertEqual(maintenance.control_plane(), "b-starting")
            del pods["b-starting"]
            with self.assertRaises(NoControlPlane):
                maintenance.control_plane()

    def test_local_failed_install_can_stop_and_uninstall_without_cp(self):
        nodes = [{"Id": "owned", "State": {"Running": True}}]
        stopped = [{"Id": "owned", "State": {"Running": False}}]
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            with patch.object(cluster, "owned_nodes", side_effect=[nodes, stopped]), patch.object(cluster, "kube", return_value='{"items": []}'), patch("kubernetes.run") as command:
                cluster.stop()
                command.assert_called_once_with("docker", "stop", "--timeout", "60", "owned")
            self.assertFalse((cluster.state / "maintenance.json").exists())
            with patch.object(cluster, "owned_nodes", return_value=nodes), patch.object(cluster, "kube", return_value='{"items": []}'), patch("kubernetes.run", return_value="") as command:
                cluster.uninstall()
                self.assertIn("delete", command.call_args_list[0].args)

    def test_install_can_resume_nodes_before_repairing_missing_workloads(self):
        cluster = k8s.LocalCluster("hibana-check")
        with patch.object(cluster, "owned_nodes", return_value=[{"Id": "owned", "State": {"Running": False}}]), patch("kubernetes.run"), patch.object(cluster, "kube") as kube, patch.object(cluster, "wait_workloads") as wait, patch.object(cluster, "resume_admission") as resume:
            cluster.start(wait_runtime=False)
            wait.assert_not_called()
            resume.assert_not_called()
            self.assertFalse(any("rollout" in c.args for c in kube.call_args_list))

    def test_failed_drain_does_not_scale_or_open_admission(self):
        from existing import ExistingCluster
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit")
            record = {"resources": [], "paused": {"owner": "review", "drained": False,
                      "replicas": {"hibana-control-plane": 2, "hibana-worker": 2}, "hpas": []}}
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "save"), patch.object(cluster, "kube") as kube, patch("existing.Maintenance") as maintenance:
                maintenance.return_value.drain.side_effect = ValueError("busy")
                with self.assertRaisesRegex(ValueError, "busy"):
                    cluster.stop()
                self.assertFalse(any("scale" in c.args for c in kube.call_args_list))
                self.assertFalse(record["paused"]["drained"])
                maintenance.return_value.open.assert_not_called()

    def test_failed_preparation_keeps_pause_owner_and_gate_closed(self):
        from existing import ExistingCluster
        with tempfile.NamedTemporaryFile() as config:
            cluster = ExistingCluster(config.name, "explicit")
            record = {"resources": [], "paused": {"owner": "review", "drained": True,
                      "replicas": {"hibana-control-plane": 2, "hibana-worker": 2}, "hpas": []}}
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "save"), patch.object(cluster, "kube"), patch.object(cluster, "ready"), patch("existing.Maintenance") as maintenance:
                maintenance.return_value.prepare.side_effect = ValueError("cache full")
                with self.assertRaisesRegex(ValueError, "cache full"):
                    cluster.start()
                maintenance.return_value.open.assert_not_called()
                self.assertEqual(record["paused"]["owner"], "review")
                self.assertFalse(record["paused"]["drained"])

    def test_drain_checks_replacement_pods_and_durable_execution_count(self):
        from maintenance import Maintenance
        maintenance = Maintenance(MagicMock())
        def pod(name):
            return {"metadata": {"name": name}, "status": {"containerStatuses": [
                {"name": "control-plane", "state": {"running": {}}}]}}
        first = {"a": pod("first")}
        second = {"b": pod("second")}
        statuses = [{"active_requests": 0, "inflight_executions": 1},
                    {"active_requests": 0, "inflight_executions": 0},
                    {"active_requests": 0, "inflight_executions": 0}]
        with patch.object(maintenance, "pods", side_effect=[first, first, second, second, second]), patch.object(maintenance, "call", side_effect=[json.dumps(s) for s in statuses]) as call, patch("maintenance.time.sleep"):
            maintenance.drain()
        self.assertEqual(call.call_count, 3)
        self.assertEqual(call.call_args.args[0], "second")

    def test_local_stop_targets_only_owned_running_nodes(self):
        cluster = k8s.LocalCluster("hibana-check")
        nodes = [{"Id": "owned-a", "State": {"Running": True}}, {"Id": "owned-b", "State": {"Running": False}}]
        stopped = [{"Id": n["Id"], "State": {"Running": False}} for n in nodes]
        with patch.object(cluster, "owned_nodes", side_effect=[nodes, stopped]), patch.object(cluster, "pause_admission") as drain, patch("kubernetes.run") as command:
            cluster.stop()
        drain.assert_called_once()
        command.assert_called_once_with("docker", "stop", "--timeout", "60", "owned-a")

    def test_local_legacy_stop_uses_grace_period_without_creating_pause_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            nodes = [{"Id": "owned", "State": {"Running": True}}]
            stopped = [{"Id": "owned", "State": {"Running": False}}]
            with patch.object(cluster, "owned_nodes", side_effect=[nodes, stopped]), patch("kubernetes.Maintenance") as maintenance, patch("kubernetes.run") as command:
                maintenance.return_value.supports_protocol.return_value = False
                cluster.stop()
                maintenance.return_value.close.assert_not_called()
                maintenance.return_value.drain.assert_not_called()
                command.assert_called_once_with("docker", "stop", "--timeout", "60", "owned")
            self.assertFalse((cluster.state / "maintenance.json").exists())

    def test_local_legacy_start_preserves_existing_owner_without_running_maintenance(self):
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            path = cluster.state / "maintenance.json"
            before = '{"owner":"old-cli-owner"}'
            path.write_text(before)
            with patch.object(cluster, "owned_nodes", return_value=[{"Id": "owned", "Name": "/hibana-check-control-plane", "State": {"Running": False}}]), patch("kubernetes.run") as command, patch.object(cluster, "kube"), patch.object(cluster, "wait_workloads") as ready, patch("kubernetes.Maintenance") as maintenance:
                maintenance.return_value.supports_protocol.return_value = False
                cluster.start()
                ready.assert_called_once()
                command.assert_called_once_with("docker", "start", "owned")
                for action in ("close", "prepare", "open"):
                    getattr(maintenance.return_value, action).assert_not_called()
            self.assertEqual(path.read_text(), before)
            # After an upgrade, reopen the saved owner and remove it only on success.
            with patch("kubernetes.Maintenance") as maintenance:
                maintenance.return_value.supports_protocol.return_value = True
                cluster.resume_admission()
                maintenance.return_value.close.assert_called_once_with("old-cli-owner")
                maintenance.return_value.prepare.assert_called_once_with("old-cli-owner")
                maintenance.return_value.open.assert_called_once_with("old-cli-owner")
            self.assertFalse(path.exists())

    def test_local_probe_failure_cannot_stop_nodes_or_create_pause_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            with patch.object(cluster, "owned_nodes", return_value=[{"Id": "owned", "State": {"Running": True}}]), patch("kubernetes.Maintenance") as maintenance, patch("kubernetes.run") as command:
                maintenance.return_value.supports_protocol.side_effect = subprocess.CalledProcessError(1, "kubectl")
                with self.assertRaises(subprocess.CalledProcessError):
                    cluster.stop()
                command.assert_not_called()
                maintenance.return_value.close.assert_not_called()
            self.assertFalse((cluster.state / "maintenance.json").exists())

    def test_local_ambiguous_close_keeps_owner_for_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            with patch("kubernetes.Maintenance") as maintenance:
                maintenance.return_value.supports_protocol.return_value = True
                maintenance.return_value.close.side_effect = subprocess.TimeoutExpired("kubectl", 25)
                with self.assertRaises(subprocess.TimeoutExpired):
                    cluster.pause_admission()
                owner = json.loads((cluster.state / "maintenance.json").read_text())["owner"]
                maintenance.return_value.close.assert_called_once_with(owner)
                maintenance.return_value.drain.assert_not_called()

    def test_local_start_waits_for_live_api_before_ready_checks(self):
        cluster = k8s.LocalCluster("hibana-check")
        with patch.object(cluster, "owned_nodes", return_value=[{"Id": "owned", "Name": "/hibana-check-control-plane", "State": {"Running": False}}]), patch("kubernetes.run") as command, patch.object(cluster, "kube") as kube, patch.object(cluster, "wait_workloads"), patch("kubernetes.Maintenance") as maintenance:
            cluster.start()
        maintenance.return_value.prepare.assert_called_once_with()
        maintenance.return_value.close.assert_not_called()
        maintenance.return_value.open.assert_not_called()
        command.assert_called_once_with("docker", "start", "owned")
        self.assertIn("/readyz", kube.call_args_list[0].args)
        self.assertEqual(sum("rollout" in call.args for call in kube.call_args_list), 2)

    def test_local_unprepared_applications_cannot_complete_startup(self):
        with tempfile.TemporaryDirectory() as directory:
            cluster = k8s.LocalCluster("hibana-check")
            cluster.state = Path(directory)
            with patch("kubernetes.Maintenance") as maintenance:
                maintenance.return_value.prepare.side_effect = ValueError("not prepared")
                with self.assertRaisesRegex(ValueError, "not prepared"):
                    cluster.resume_admission()
                maintenance.return_value.open.assert_not_called()

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
            with patch.object(cluster, "owned_nodes", return_value=[]), patch("kubernetes.run", side_effect=["", "leftover"]):
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
            with patch.object(cluster, "record", return_value=record), patch.object(cluster, "get", side_effect=deployment), patch.object(cluster, "kube", side_effect=kube), patch.object(cluster, "save", side_effect=lambda r: events.append(("save", deepcopy(r)))) as save, patch.object(cluster, "ready"), patch.object(cluster, "apply") as apply, patch("existing.Maintenance") as maintenance:
                for action in ("close", "drain", "prepare", "open"):
                    getattr(maintenance.return_value, action).side_effect = lambda *args, action=action: events.append((action,))
                cluster.stop()
                self.assertEqual(events[1][0], "save", "resume settings must be saved before scaling")
                self.assertEqual(record["paused"]["replicas"], {"hibana-control-plane": 2, "hibana-worker": 4})
                scaled = [e for e in events if "scale" in e]
                self.assertIn("deployment/hibana-worker", scaled[0])
                self.assertLess(events.index(("drain",)), events.index(scaled[0]))
                cluster.start()
                self.assertNotIn("paused", record)
                apply.assert_called_once_with([hpa])
                self.assertIn(("-n", "hibana", "scale", "deployment/hibana-worker", "--replicas=4"), events)
                self.assertLess(events.index(("prepare",)), events.index(("open",)))


if __name__ == "__main__":
    unittest.main()
