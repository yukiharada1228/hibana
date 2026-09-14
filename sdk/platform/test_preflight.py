"""Exercise installation checks, read-only previews and recovery against a fake API."""
from copy import deepcopy
import base64
import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch, MagicMock

from existing import ExistingCluster, LABEL, MARKER
from common import stamp_runtime_settings
from network import NetworkTransition, PREFIX as NETWORK_PREFIX
from operation import NAME as OPERATION_LOCK
from readiness import Readiness
from preflight import Preflight, comparable, fields_changed, resource_id


def resource(kind, name, **fields):
    return {"apiVersion": "v1", "kind": kind, "metadata": {"name": name, **({} if kind == "Namespace" else {"namespace": "hibana"}),
        "labels": {"app.kubernetes.io/part-of": "hibana", LABEL: "hibana"}}, **fields}


class FakeCluster(ExistingCluster):
    def __init__(self, directory):
        root = Path(directory)
        (root / "config").touch()
        (root / "migration").mkdir()
        super().__init__(root / "config", "test", root, "registry.test/hibana:v1")
        self.docs = [resource("Namespace", "hibana"), resource("ConfigMap", "hibana-config", data={
            "S3_ENDPOINT": "https://objects.test", "S3_BUCKET": "bucket", "INGRESS_BASE_DOMAIN": "apps.test"}),
            resource("Secret", "hibana-runtime", stringData={"DATABASE_URL": "postgres://private-credential@db/hibana"}),
            resource("Secret", "hibana-control-plane", stringData={key: "private-credential" for key in
                ["REDIS_URL", "S3_ACCESS_KEY", "S3_SECRET_KEY", "BOOTSTRAP_ADMIN_TOKEN", "JOB_SIGNING_KEY", "SECRETS_MASTER_KEY"]}),
            resource("Secret", "hibana-migration", stringData={"MIGRATION_DATABASE_URL": "postgres://private-admin@db/hibana"})]
        self.docs[3]["stringData"]["REDIS_URL"] = "redis://private-credential@redis:6379"
        for name in ("control-plane", "worker"):
            refs = ["hibana-runtime"] + (["hibana-control-plane"] if name == "control-plane" else [])
            self.docs.append(resource("Deployment", "hibana-" + name, spec={"replicas": 2, "template": {"spec": {"containers": [{
                "name": name, "image": self.image, "envFrom": [{"configMapRef": {"name": "hibana-config"}}] + [{"secretRef": {"name": ref}} for ref in refs]}]}}}))
        self.migration = [resource("Job", "hibana-migrate", spec={"template": {"spec": {"containers": [{"name": "migrate", "image": self.image,
            "env": [{"name": "MIGRATION_DATABASE_URL", "valueFrom": {"secretKeyRef": {"name": "hibana-migration", "key": "MIGRATION_DATABASE_URL"}}}]}]}}})]
        self.live, self.events, self.denied = {}, [], set()
        self.fail_server = None
        self.fail_wait = False
        self.fail_rollout = False
        self.api_mutex = threading.Lock()

    def render(self, path, image):
        return deepcopy(self.migration if Path(path).name == "migration" else self.docs)

    def verify_dependencies(self):
        self.events.append(("dependencies", deepcopy(self.live)))

    def get(self, kind, name):
        with self.api_mutex:
            key = next((key for key in self.live if key[0].lower() == kind.lower() and key[1] == name), None)
            return deepcopy(self.live.get(key))

    def apply(self, docs):
        self.events.append(("mutation", deepcopy(docs)))
        for doc in docs:
            self.live[resource_id(doc)] = deepcopy(doc)
            if doc["kind"] == "Job":
                self.live[resource_id(doc)]["status"] = {"succeeded": 1}

    def kube(self, *args, **kwargs):
        self.events.append(("kubectl", args, kwargs))
        if "auth" in args:
            if any(value in args for value in self.denied):
                raise subprocess.CalledProcessError(1, ["kubectl", *args])
            return "yes\n"
        if "--dry-run=server" in args:
            doc = json.loads(kwargs["input"])
            if doc["kind"] == self.fail_server:
                raise subprocess.CalledProcessError(1, "kubectl", stderr="private-credential")
            if args[0] == "apply" and doc["kind"] == "Job":
                before = self.live.get(resource_id(doc))
                if before and before["spec"]["template"] != doc["spec"]["template"]:
                    raise subprocess.CalledProcessError(1, "kubectl", stderr="spec.template: field is immutable")
            if args[0] == "create":
                doc["metadata"]["name"] = "hibana-migrate-check-test"
            return json.dumps(doc)
        if args[0] in ("create", "replace"):
            with self.api_mutex:
                doc = json.loads(kwargs["input"])
                before = self.live.get(resource_id(doc))
                if ((args[0] == "create" and before) or (args[0] == "replace" and
                        (not before or any(doc["metadata"].get(key) != before["metadata"].get(key)
                                           for key in ("resourceVersion", "uid"))))):
                    raise subprocess.CalledProcessError(1, args, stderr="Conflict")
                doc["metadata"].update(resourceVersion=uuid.uuid4().hex,
                                       uid=before["metadata"]["uid"] if before else uuid.uuid4().hex)
                self.live[resource_id(doc)] = deepcopy(doc)
                if doc["metadata"]["name"] != OPERATION_LOCK:
                    self.events.append(("mutation", [deepcopy(doc)]))
                return json.dumps(doc)
        if "scale" in args:
            name = args[args.index("scale") + 1].split("/")[1]
            self.live[("Deployment", name)]["spec"]["replicas"] = int(next(a.split("=")[1] for a in args if a.startswith("--replicas=")))
        if ("wait" in args and self.fail_wait) or ("rollout" in args and self.fail_rollout):
            raise subprocess.CalledProcessError(1, ["kubectl", *args])
        if "get" in args and ("pods" in args or "hpa" in args):
            return json.dumps({"items": []})
        if "get" in args and "networkpolicies" in args:
            return json.dumps({"items": [d for (kind, _), d in self.live.items() if kind == "NetworkPolicy"]})
        if "delete" in args:
            offset = args.index("delete")
            kind, name = args[offset + 1:offset + 3]
            for key in list(self.live):
                if key[0].lower() == kind.lower() and key[1] == name:
                    del self.live[key]
        return "{}"


class ClusterFixture(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.cluster = FakeCluster(temp.name)
        tools = patch("preflight.shutil.which", return_value="kubectl")
        tools.start()
        self.addCleanup(tools.stop)

    def output(self, call):
        stream = io.StringIO()
        with contextlib.redirect_stdout(stream):
            call()
        return stream.getvalue()

    def assert_read_only(self):
        self.assertFalse(any(e[0] == "mutation" for e in self.cluster.events))
        for event in self.cluster.events:
            args = event[1]
            if any(verb in args for verb in ("apply", "create", "delete", "scale", "patch")) and "auth" not in args:
                self.assertIn("--dry-run=server", args)


class PreflightTests(ClusterFixture):
    def settings_hash(self, docs=None):
        docs = deepcopy(docs or self.cluster.docs)
        stamp_runtime_settings(docs, [])
        return next(d for d in docs if resource_id(d) == ("Deployment", "hibana-worker"))["spec"]["template"]["metadata"]["annotations"]["hibana.local/settings-hash"]

    def test_direct_secret_and_config_keys_restart_only_their_consumers(self):
        pod = self.cluster.docs[-1]["spec"]["template"]["spec"]
        for kind, ref_key, data_field in (("Secret", "secretKeyRef", "stringData"), ("ConfigMap", "configMapKeyRef", "data")):
            with self.subTest(kind=kind):
                source = resource(kind, "hibana-direct", **{data_field: {"selected": "old", "unused": "old"}})
                docs = self.cluster.docs + [source]
                pod["containers"][0]["env"] = [{"name": "EXTRA", "valueFrom": {ref_key: {"name": "hibana-direct", "key": "selected"}}}]
                before = self.settings_hash(docs)
                source[data_field]["unused"] = "new"
                self.assertEqual(before, self.settings_hash(docs))
                source[data_field]["selected"] = "new"
                self.assertNotEqual(before, self.settings_hash(docs))

    def test_mixed_secret_data_is_merged_and_representation_does_not_restart(self):
        secret = self.cluster.docs[2]
        value = secret["stringData"]["DATABASE_URL"]
        before = self.settings_hash()
        secret["stringData"] = {}
        encoded = base64.b64encode(value.encode()).decode()
        secret["data"] = {"DATABASE_URL": "\r\n".join(encoded[i:i + 10] for i in range(0, len(encoded), 10))}
        self.assertEqual(before, self.settings_hash())
        secret["stringData"]["EXTRA"] = "constant"
        before = self.settings_hash()
        secret["data"]["DATABASE_URL"] = base64.b64encode(b"postgres://new-db/hibana").decode()
        self.assertNotEqual(before, self.settings_hash())
        secret["stringData"]["DATABASE_URL"] = value
        before = self.settings_hash()
        secret["data"]["DATABASE_URL"] = base64.b64encode(b"ignored").decode()
        self.assertEqual(before, self.settings_hash())

    def test_effective_environment_hash_respects_prefix_order_and_expansion(self):
        worker = self.cluster.docs[-1]["spec"]["template"]["spec"]["containers"][0]
        config = self.cluster.docs[1]["data"]
        worker["envFrom"].append({"configMapRef": {"name": "hibana-config"}, "prefix": "PREFIX_"})
        worker["env"] = [{"name": "S3_BUCKET", "value": "fixed"}, {"name": "PREFIX_S3_BUCKET", "value": "fixed"}]
        before = self.settings_hash()
        config["S3_BUCKET"] = "changed"
        self.assertEqual(before, self.settings_hash())
        worker["env"].insert(0, {"name": "EXPANDED", "value": "$(PREFIX_S3_BUCKET)/$$(S3_BUCKET)"})
        before = self.settings_hash()
        config["S3_BUCKET"] = "changed-again"
        self.assertNotEqual(before, self.settings_hash())

    def test_init_container_and_optional_key_appearance_change_hash(self):
        pod = self.cluster.docs[-1]["spec"]["template"]["spec"]
        pod["initContainers"] = [{"name": "init", "env": [{"name": "EXTRA", "valueFrom": {"secretKeyRef": {
            "name": "hibana-optional", "key": "text", "optional": True}}}]}]
        before = self.settings_hash()
        secret = resource("Secret", "hibana-optional", data={"unused": "//4="})
        self.cluster.docs.append(secret)
        self.assertEqual(before, self.settings_hash())
        secret["stringData"] = {"text": "now-present"}
        self.assertNotEqual(before, self.settings_hash())

    def test_external_direct_secret_rotation_is_detected_during_preflight(self):
        worker = self.cluster.docs[-1]["spec"]["template"]["spec"]["containers"][0]
        worker["env"] = [{"name": "DATABASE_URL", "valueFrom": {"secretKeyRef": {"name": "database", "key": "url"}}}]
        secret = resource("Secret", "database", stringData={"url": "postgres://old-db/hibana"})
        self.cluster.live[resource_id(secret)] = secret
        with contextlib.redirect_stdout(io.StringIO()):
            before = self.cluster.prepare_install()[0][-1]["spec"]["template"]
            secret["stringData"]["url"] = "postgres://new-db/hibana"
            after = self.cluster.prepare_install()[0][-1]["spec"]["template"]
        self.assertNotEqual(before, after)
        self.assert_read_only()

    def test_optional_key_references_preserve_existing_environment_values(self):
        worker = self.cluster.docs[-1]["spec"]["template"]["spec"]["containers"][0]
        for kind, ref_key in (("Secret", "secretKeyRef"), ("ConfigMap", "configMapKeyRef")):
            for present in (False, True):
                with self.subTest(kind=kind, present=present):
                    self.cluster.live.clear()
                    if present:
                        self.cluster.live[(kind, "hibana-optional")] = resource(kind, "hibana-optional", data={})
                    worker["env"] = [
                        {"name": "EXTRA", "valueFrom": {ref_key: {"name": "hibana-optional", "key": "extra", "optional": True}}},
                        {"name": "DATABASE_URL", "valueFrom": {ref_key: {"name": "hibana-optional", "key": "database", "optional": True}}},
                    ]
                    self.output(self.cluster.prepare_install)
                    self.assert_read_only()

    def test_optional_reference_does_not_replace_required_environment_validation(self):
        self.cluster.docs[-1]["spec"]["template"]["spec"]["containers"][0].update(
            envFrom=[], env=[{"name": "DATABASE_URL", "valueFrom": {"secretKeyRef": {
                "name": "hibana-optional", "key": "database", "optional": True}}}])
        with self.assertRaisesRegex(ValueError, "missing environment setting DATABASE_URL") as caught:
            self.cluster.prepare_install()
        self.assertNotIn("Missing Secret/hibana-optional", str(caught.exception))
        self.assert_read_only()

    def test_missing_required_key_is_rejected_but_empty_extra_value_is_valid(self):
        worker = self.cluster.docs[-1]["spec"]["template"]["spec"]["containers"][0]
        self.cluster.docs.append(resource("Secret", "hibana-extra", stringData={"empty": ""}))
        worker["env"] = [{"name": "EXTRA", "valueFrom": {"secretKeyRef": {"name": "hibana-extra", "key": "empty"}}}]
        self.output(self.cluster.prepare_install)
        worker["env"][0]["valueFrom"]["secretKeyRef"]["key"] = "missing"
        with self.assertRaisesRegex(ValueError, "Missing Secret/hibana-extra key missing"):
            self.cluster.prepare_install()
        self.assert_read_only()

    def test_binary_secret_volumes_and_unselected_data_are_allowed(self):
        pod = self.cluster.docs[-1]["spec"]["template"]["spec"]
        self.cluster.docs.append(resource("Secret", "hibana-keystore", data={
            "truststore.p12": base64.b64encode(b"\x00\xff\xfe").decode(), "text": base64.b64encode(b"ok").decode()}))
        pod["volumes"] = [{"name": "keystore", "secret": {"secretName": "hibana-keystore"}}]
        pod["containers"][0]["env"] = [{"name": "EXTRA", "valueFrom": {"secretKeyRef": {"name": "hibana-keystore", "key": "text"}}}]
        self.output(self.cluster.prepare_install)
        pod["containers"][0]["env"][0]["valueFrom"]["secretKeyRef"]["key"] = "truststore.p12"
        with self.assertRaisesRegex(ValueError, "Secret/hibana-keystore has invalid text data"):
            self.cluster.prepare_install()
        self.assert_read_only()

    def test_string_data_override_is_used_before_decoding_secret_text(self):
        secret = self.cluster.docs[2]
        secret["data"] = {"DATABASE_URL": base64.b64encode(b"\xff").decode()}
        self.output(self.cluster.prepare_install)
        self.assert_read_only()

    def test_kustomize_wrapped_base64_is_accepted_and_matches_server_data(self):
        secret = self.cluster.docs[2]
        value = secret.pop("stringData")["DATABASE_URL"]
        encoded = base64.b64encode(value.encode()).decode()
        secret["data"] = {"DATABASE_URL": "\r\n".join(encoded[i:i + 14] for i in range(0, len(encoded), 14)) + "\n"}
        self.output(self.cluster.prepare_install)
        server = deepcopy(secret)
        server["data"]["DATABASE_URL"] = encoded
        self.assertEqual(comparable(secret), comparable(server))
        secret["data"]["DATABASE_URL"] = encoded + "!"
        with self.assertRaisesRegex(ValueError, "has invalid text data"):
            self.cluster.prepare_install()
        self.assert_read_only()

    def test_tls_keys_are_checked_without_decoding_unrelated_binary_data(self):
        self.cluster.docs.append(resource("Ingress", "hibana-management", spec={"tls": [{"secretName": "site-cert"}]}))
        self.cluster.live[("Secret", "site-cert")] = resource("Secret", "site-cert", data={
            "tls.crt": base64.b64encode(b"certificate").decode(), "tls.key": base64.b64encode(b"private-key").decode(),
            "keystore.p12": base64.b64encode(b"\xff\xfe").decode()})
        self.output(self.cluster.prepare_install)
        self.cluster.live[("Secret", "site-cert")]["data"]["tls.key"] = base64.b64encode(b"  ").decode()
        with self.assertRaisesRegex(ValueError, "Missing Secret/site-cert key tls.key"):
            self.cluster.prepare_install()
        self.assert_read_only()

    def test_additional_job_is_validated_as_an_update_before_any_mutation(self):
        job = resource("Job", "hibana-setup", spec={"template": {"spec": {"containers": [{"name": "setup", "image": "setup:v2"}]}}})
        self.cluster.docs.append(job)
        self.cluster.live[("Namespace", "hibana")] = deepcopy(self.cluster.docs[0])
        self.cluster.live[resource_id(job)] = deepcopy(job)
        self.cluster.live[resource_id(job)]["spec"]["template"]["spec"]["containers"][0]["image"] = "setup:v1"
        for action in (lambda: self.cluster.preview("install"), self.cluster.install):
            with self.assertRaisesRegex(ValueError, "rejected Job/hibana-setup"):
                self.output(action)
            self.assert_read_only()

    def test_unchanged_additional_job_is_not_recreated_or_given_delete_permissions(self):
        job = resource("Job", "hibana-setup", spec={"template": {"spec": {"containers": [{"name": "setup", "image": "setup:v1"}]}}})
        self.cluster.docs.append(job)
        self.cluster.live[("Namespace", "hibana")] = deepcopy(self.cluster.docs[0])
        self.cluster.live[resource_id(job)] = deepcopy(job)
        output = self.output(lambda: self.cluster.preview("install"))
        self.assertIn("unchanged Job/hibana-setup", output)
        self.assertFalse(any(e[0] == "kubectl" and "auth" in e[1] and "delete" in e[1] and "jobs.batch/hibana-setup" in e[1] for e in self.cluster.events))
        self.assert_read_only()

    def test_fresh_preview_checks_access_and_defers_only_namespaced_server_validation(self):
        output = self.output(lambda: self.cluster.preview("install"))
        self.assertIn("create    Namespace/hibana", output)
        self.assertIn("Namespaced server validation", output)
        self.assertNotIn("private-credential", output)
        self.assertTrue(any("auth" in e[1] for e in self.cluster.events))
        self.assert_read_only()

    def test_existing_preview_validates_on_server_and_reports_actual_changed_fields_without_values(self):
        for doc in self.cluster.docs + self.cluster.migration:
            self.cluster.live[resource_id(doc)] = deepcopy(doc)
        self.cluster.live[("Job", "hibana-migrate")]["status"] = {"succeeded": 1}
        self.cluster.live[("Secret", "hibana-runtime")]["stringData"]["DATABASE_URL"] = "private-old-value"
        output = self.output(lambda: self.cluster.preview("install"))
        self.assertIn("/data/DATABASE_URL", output)
        self.assertIn("recreate  Job/hibana-migrate", output)
        self.assertNotIn("private-old-value", output)
        self.assertNotIn("private-credential", output)
        self.assertNotIn("Namespaced server validation", output)
        self.assert_read_only()

    def test_missing_settings_and_migration_references_abort_before_mutations(self):
        self.cluster.docs = [d for d in self.cluster.docs if d["metadata"]["name"] != "hibana-migration"]
        self.cluster.docs[1]["data"]["S3_ENDPOINT"] = "https://CHANGE_ME"
        with self.assertRaisesRegex(ValueError, "Missing Secret/hibana-migration") as caught:
            self.cluster.install()
        self.assertIn("replace the example setting", str(caught.exception))
        self.assertNotIn("private-credential", str(caught.exception))
        self.assert_read_only()

    def test_permission_denial_and_connectivity_failure_abort_before_mutations(self):
        self.cluster.denied.add("deployments.apps")
        with self.assertRaisesRegex(ValueError, "permissions are missing"):
            self.cluster.install()
        self.assert_read_only()
        with patch.object(self.cluster, "kube", side_effect=subprocess.CalledProcessError(1, "kubectl")):
            with self.assertRaisesRegex(ValueError, "Cannot reach"):
                self.cluster.install()
        self.assert_read_only()

    def test_existing_server_rejection_hides_error_payload_and_keeps_state(self):
        self.cluster.live[("Namespace", "hibana")] = self.cluster.docs[0]
        self.cluster.fail_server = "Secret"
        with self.assertRaisesRegex(ValueError, "rejected Secret/hibana-runtime") as caught:
            self.cluster.install()
        self.assertNotIn("private-credential", str(caught.exception))
        self.assert_read_only()

    def test_failed_migration_is_detected_before_configuration_can_change(self):
        self.cluster.live[("Job", "hibana-migrate")] = resource("Job", "hibana-migrate", status={"failed": 1})
        with self.assertRaisesRegex(ValueError, "retained for inspection"):
            self.cluster.install()
        self.assert_read_only()

    def test_secret_encodings_and_server_metadata_do_not_create_spurious_diff(self):
        before = resource("Secret", "hibana-key", data={"key": "dmFsdWU="})
        before["metadata"].update(resourceVersion="1", uid="id", managedFields=[])
        after = resource("Secret", "hibana-key", stringData={"key": "value"})
        self.assertEqual(fields_changed(comparable(before), comparable(after)), [])

    def test_referenced_tls_and_direct_environment_secret_keys_are_checked(self):
        self.cluster.docs.append(resource("Ingress", "hibana-management", spec={"tls": [{"secretName": "site-cert"}]}))
        with self.assertRaisesRegex(ValueError, "Missing Secret/site-cert"):
            self.cluster.prepare_install()
        self.assert_read_only()

    def test_invalid_dependency_url_is_reported_without_its_credentials(self):
        self.cluster.docs[2]["stringData"]["DATABASE_URL"] = "private-malformed-credential"
        with self.assertRaisesRegex(ValueError, "DATABASE_URL must be a valid") as caught:
            self.cluster.prepare_install()
        self.assertNotIn("private-malformed-credential", str(caught.exception))
        self.assert_read_only()

    def test_kubectl_render_and_apply_errors_do_not_print_secret_payloads(self):
        from common import KubernetesTarget
        target = KubernetesTarget()
        with patch.object(target, "run", side_effect=subprocess.CalledProcessError(1, "kubectl", stderr="private-credential")):
            with self.assertRaisesRegex(ValueError, "Could not render") as caught:
                target.render("site", "image")
        self.assertNotIn("private-credential", str(caught.exception))
        with patch.object(target, "kube", side_effect=subprocess.CalledProcessError(1, "kubectl", stderr="private-credential")) as kube:
            with self.assertRaisesRegex(ValueError, "could not apply Secret/hibana-runtime") as caught:
                target.apply([self.cluster.docs[2]])
        self.assertTrue(kube.call_args.kwargs["quiet"])
        self.assertNotIn("private-credential", str(caught.exception))


class RemoteInstallTests(ClusterFixture):
    def test_terminating_old_pod_keeps_old_network_access_until_it_exits(self):
        policy = self.network_update()
        old = {"metadata": {"name": "old-worker", "deletionTimestamp": "now"}, "status": {"phase": "Running"}}
        calls = []
        def pending(checks):
            calls.append(True)
            self.assertTrue(self.temporary_policies())
            self.assertEqual(self.cluster.live[resource_id(policy)]["spec"]["egress"][0]["to"][0]["ipBlock"]["cidr"], "192.0.2.10/32")
            return [old] if len(calls) == 1 else []
        with patch.object(Readiness, "terminating", pending), patch("readiness.time.sleep"):
            self.output(self.cluster.install)
        self.assertEqual(len(calls), 2)
        self.assertEqual(self.cluster.live[resource_id(policy)]["spec"], policy["spec"])

    def test_old_pod_timeout_retains_policies_and_retry_recovers(self):
        self.network_update()
        with patch.object(Readiness, "wait_termination", side_effect=ValueError("old Pod timed out")):
            with self.assertRaisesRegex(ValueError, "old Pod timed out"):
                self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["phase"], "old Pod termination")
        self.assertTrue(self.temporary_policies())
        self.assertFalse(any(e[0] == "dependencies" for e in self.cluster.events))
        self.output(self.cluster.install)
        self.assertEqual(self.temporary_policies(), [])

    def test_final_policy_failure_without_ingress_is_recorded_and_can_be_repaired(self):
        policy = self.network_update()
        policy["spec"]["egress"] = []
        def verify():
            self.assertEqual(self.cluster.live[resource_id(policy)]["spec"]["egress"], [])
            raise ValueError("worker database is blocked")
        with patch.object(self.cluster, "verify_dependencies", side_effect=verify):
            with self.assertRaisesRegex(ValueError, "worker database is blocked"):
                self.output(self.cluster.install)
        record = self.cluster.record()["install"]
        self.assertEqual((record["status"], record["phase"]), ("failed", "dependency verification"))
        self.assertNotIn("management API check", record["completed"])
        policy["spec"]["egress"] = [{"to": [{"ipBlock": {"cidr": "192.0.2.20/32"}}]}]
        self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["status"], "complete")

    def test_auxiliary_deployment_is_ready_before_final_policy_and_exec_rbac_is_preflighted(self):
        self.cluster.docs.append(resource("Deployment", "hibana-extra", spec={"template": {"spec": {"containers": []}}}))
        self.cluster.denied.add("pods/exec")
        with self.assertRaisesRegex(ValueError, "permissions are missing"):
            self.output(self.cluster.install)
        self.assert_read_only()
        self.cluster.denied.clear()
        self.output(self.cluster.install)
        self.assertTrue(any(e[0] == "kubectl" and "rollout" in e[1] and "deployment/hibana-extra" in e[1] for e in self.cluster.events))

    def network_update(self):
        self.cluster.live[("Namespace", "hibana")] = deepcopy(self.cluster.docs[0])
        self.cluster.live[resource_id(self.cluster.docs[-1])] = deepcopy(self.cluster.docs[-1])
        deny = resource("NetworkPolicy", "default-deny", spec={"podSelector": {}, "policyTypes": ["Ingress", "Egress"]})
        policy = resource("NetworkPolicy", "hibana-dependencies", spec={"podSelector": {"matchLabels": {"app": "hibana"}},
            "policyTypes": ["Egress"], "egress": [{"to": [{"ipBlock": {"cidr": "192.0.2.10/32"}}], "ports": [{"port": 5432}]}]})
        self.cluster.live[resource_id(deny)] = deepcopy(deny)
        self.cluster.live[resource_id(policy)] = deepcopy(policy)
        policy["spec"]["egress"][0]["to"][0]["ipBlock"]["cidr"] = "192.0.2.20/32"
        self.cluster.docs.extend([deny, policy])
        return policy

    def temporary_policies(self):
        return [d for (k, n), d in self.cluster.live.items() if k == "NetworkPolicy" and n.startswith(NETWORK_PREFIX)]

    def test_new_network_access_precedes_migration_and_old_access_survives_rollout(self):
        policy = self.network_update()
        original_kube = self.cluster.kube
        observations = []
        def kube(*args, **kwargs):
            if "auth" not in args and ("wait" in args or "rollout" in args):
                live = self.cluster.live[resource_id(policy)]
                self.assertEqual(live["spec"]["egress"][0]["to"][0]["ipBlock"]["cidr"], "192.0.2.10/32")
                self.assertTrue(self.temporary_policies())
                for temporary in self.temporary_policies():
                    self.assertEqual(temporary["spec"]["egress"], policy["spec"]["egress"])
                observations.append(args)
            return original_kube(*args, **kwargs)
        with patch.object(self.cluster, "kube", side_effect=kube):
            self.output(self.cluster.install)
        self.assertEqual(len(observations), 3)
        self.assertEqual(self.cluster.live[resource_id(policy)]["spec"], policy["spec"])
        self.assertEqual(self.temporary_policies(), [])
        self.assertFalse(any(r["name"].startswith(NETWORK_PREFIX) for r in self.cluster.record()["resources"]))

    def test_failed_rollout_retains_network_access_and_retry_cleans_it_up(self):
        policy = self.network_update()
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.output(self.cluster.install)
        names = {d["metadata"]["name"] for d in self.temporary_policies()}
        self.assertTrue(names)
        self.assertTrue(names <= {r["name"] for r in self.cluster.record()["resources"]})
        output = self.output(lambda: self.cluster.preview("install"))
        self.assertIn("Temporary network access", output)
        self.assertEqual(names, {d["metadata"]["name"] for d in self.temporary_policies()})
        self.cluster.fail_rollout = False
        self.output(self.cluster.install)
        self.assertEqual(self.cluster.live[resource_id(policy)]["spec"], policy["spec"])
        self.assertEqual(self.temporary_policies(), [])

    def test_network_preview_checks_temporary_admission_and_cleanup_permissions(self):
        self.network_update()
        self.cluster.denied.add("delete")
        with self.assertRaisesRegex(ValueError, "permissions are missing"):
            self.output(self.cluster.install)
        self.assert_read_only()
        self.cluster.denied.clear()
        original_check = Preflight.server_check
        def server_check(checks, doc, **kwargs):
            if doc["metadata"]["name"].startswith(NETWORK_PREFIX):
                raise ValueError("temporary policy rejected")
            return original_check(checks, doc, **kwargs)
        with patch.object(Preflight, "server_check", server_check):
            with self.assertRaisesRegex(ValueError, "temporary policy rejected"):
                self.output(self.cluster.install)
        self.assert_read_only()

    def test_network_cleanup_failure_is_recoverable(self):
        self.network_update()
        original_kube = self.cluster.kube
        def kube(*args, **kwargs):
            if "delete" in args and "auth" not in args and any(a.startswith(NETWORK_PREFIX) for a in args):
                raise subprocess.CalledProcessError(1, args)
            return original_kube(*args, **kwargs)
        with patch.object(self.cluster, "kube", side_effect=kube):
            with self.assertRaises(subprocess.CalledProcessError):
                self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["phase"], "network policy reconciliation")
        self.assertTrue(self.temporary_policies())
        self.output(self.cluster.install)
        self.assertEqual(self.temporary_policies(), [])

    def test_network_plan_does_not_isolate_previously_unselected_pods(self):
        # Existing ingress isolation is narrower than the new selector, and
        # there is no egress isolation at all. Temporary access must not add it.
        self.cluster.live[("Namespace", "hibana")] = self.cluster.docs[0]
        old = resource("NetworkPolicy", "site-policy", spec={"podSelector": {"matchLabels": {"tier": "old"}}, "ingress": []})
        self.cluster.live[resource_id(old)] = old
        policy = resource("NetworkPolicy", "hibana-new", spec={
            "podSelector": {"matchExpressions": [{"key": "app", "operator": "In", "values": ["runtime", "migrate"]}]},
            "policyTypes": ["Ingress", "Egress"], "ingress": [{"ports": [{"port": 8080}]}], "egress": [{}]})
        plan = NetworkTransition(self.cluster, [policy], {"resources": []}, True)
        self.assertEqual(len(plan.temporary), 1)
        spec = plan.temporary[0]["spec"]
        self.assertEqual(spec["policyTypes"], ["Ingress"])
        self.assertEqual(spec["podSelector"]["matchExpressions"], [
            {"key": "app", "operator": "In", "values": ["migrate", "runtime"]},
            {"key": "tier", "operator": "In", "values": ["old"]}])
        self.assertEqual(old, self.cluster.live[resource_id(old)])
        self.assert_read_only()

    def test_network_reserved_names_and_changed_ownership_are_rejected(self):
        policy = self.network_update()
        policy["metadata"]["name"] = NETWORK_PREFIX + "user"
        with self.assertRaisesRegex(ValueError, "reserved"):
            self.output(self.cluster.install)
        self.assert_read_only()
        policy["metadata"]["name"] = "hibana-dependencies"
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.output(self.cluster.install)
        self.temporary_policies()[0]["metadata"]["labels"][LABEL] = "other"
        self.cluster.events.clear()
        with self.assertRaisesRegex(ValueError, "not owned"):
            self.output(self.cluster.install)
        self.assert_read_only()

    def test_network_transition_preserves_exact_access_when_isolation_changes(self):
        def matches(selector, labels):
            if any(labels.get(k) != v for k, v in selector.get("matchLabels", {}).items()):
                return False
            for rule in selector.get("matchExpressions", []):
                key, op, values = rule["key"], rule["operator"], rule.get("values", [])
                if op == "In" and labels.get(key) not in values or op == "NotIn" and labels.get(key) in values:
                    return False
                if op == "Exists" and key not in labels or op == "DoesNotExist" and key in labels:
                    return False
            return True

        def allowed(policies, labels, direction, port):
            selected = [p["spec"] for p in policies if direction in p["spec"]["policyTypes"] and matches(p["spec"]["podSelector"], labels)]
            return not selected or any(not rule.get("ports") or any(p["port"] == port for p in rule["ports"])
                for spec in selected for rule in spec.get(direction.lower(), []))

        original = resource("NetworkPolicy", "hibana-change", spec={"podSelector": {
            "matchExpressions": [{"key": "app", "operator": "Exists"}]},
            "policyTypes": ["Ingress", "Egress"], "ingress": [{"ports": [{"port": 8080}]}], "egress": []})
        retained = resource("NetworkPolicy", "site-policy", spec={"podSelector": {"matchLabels": {"tier": "locked"}},
            "policyTypes": ["Egress"], "egress": [{"ports": [{"port": 8081}]}]})
        self.cluster.live.update({resource_id(p): p for p in (original, retained)})
        for selector in ({"matchLabels": {"app": "runtime"}},
                         {"matchExpressions": [{"key": "app", "operator": "NotIn", "values": ["runtime"]}]},
                         {"matchExpressions": [{"key": "app", "operator": "DoesNotExist"}]}, {}):
            for types in (["Ingress"], ["Egress"], ["Ingress", "Egress"]):
                desired = resource("NetworkPolicy", "hibana-change", spec={"podSelector": selector, "policyTypes": types,
                    **{d.lower(): [{"ports": [{"port": 5432}]}] for d in types}})
                plan = NetworkTransition(self.cluster, [desired], {"resources": []}, True)
                for app in (None, "runtime", "other"):
                    for tier in (None, "locked", "open"):
                        labels = {k: v for k, v in (("app", app), ("tier", tier)) if v is not None}
                        for direction in ("Ingress", "Egress"):
                            for port in (5432, 8080, 8081, 9999):
                                with self.subTest(selector=selector, types=types, labels=labels, direction=direction, port=port):
                                    old = allowed([original, retained], labels, direction, port)
                                    final = allowed([desired, retained], labels, direction, port)
                                    self.assertEqual(allowed([original, retained, *plan.temporary], labels, direction, port), old or final)
        self.assert_read_only()

    def test_fresh_namespace_network_policies_are_applied_before_migration(self):
        policy = resource("NetworkPolicy", "default-deny", spec={"podSelector": {}, "policyTypes": ["Ingress", "Egress"]})
        self.cluster.docs.append(policy)
        original_kube = self.cluster.kube
        def kube(*args, **kwargs):
            if "wait" in args:
                self.assertEqual(self.cluster.live[resource_id(policy)], policy)
            return original_kube(*args, **kwargs)
        with patch.object(self.cluster, "kube", side_effect=kube):
            self.output(self.cluster.install)
        self.assertEqual(self.temporary_policies(), [])

    def test_precreated_namespace_is_isolated_before_first_workloads_even_on_failure(self):
        self.cluster.live[("Namespace", "hibana")] = deepcopy(self.cluster.docs[0])
        self.cluster.docs = [d for d in self.cluster.docs if d["kind"] != "Namespace"]
        # Namespace and a site-managed TLS Secret exist before Hibana installation.
        self.cluster.live[("Secret", "site-cert")] = resource("Secret", "site-cert", data={})
        policy = resource("NetworkPolicy", "default-deny", spec={"podSelector": {}, "policyTypes": ["Ingress", "Egress"]})
        self.cluster.docs.append(policy)
        original_apply = self.cluster.apply
        def apply(docs):
            if any(d["kind"] in {"Job", "Deployment"} for d in docs):
                self.assertEqual(self.cluster.get("NetworkPolicy", "default-deny"), policy)
            original_apply(docs)
        self.cluster.fail_rollout = True
        with patch.object(self.cluster, "apply", side_effect=apply):
            with self.assertRaises(subprocess.CalledProcessError):
                self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["phase"], "workload rollout")
        self.assertEqual(self.cluster.get("NetworkPolicy", "default-deny"), policy)
        self.cluster.fail_rollout = False
        self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["status"], "complete")

    def test_empty_policy_types_use_kubernetes_defaults_without_granting_all_access(self):
        old = resource("NetworkPolicy", "hibana-defaults", spec={"podSelector": {}, "policyTypes": ["Ingress", "Egress"]})
        self.cluster.live[resource_id(old)] = old
        for types in (None, []):
            with self.subTest(types=types):
                desired = resource("NetworkPolicy", "hibana-defaults", spec={"podSelector": {}, "policyTypes": types,
                    "egress": [{"ports": [{"port": 5432}]}]})
                plan = NetworkTransition(self.cluster, [desired], {"resources": []}, True)
                self.assertEqual(len(plan.temporary), 1)
                self.assertEqual(plan.temporary[0]["spec"]["egress"], [{"ports": [{"port": 5432}]}])

    def test_uninstall_removes_temporary_policies_after_failed_rollout(self):
        self.network_update()
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.output(self.cluster.install)
        self.assertTrue(self.temporary_policies())
        self.output(self.cluster.uninstall)
        self.assertEqual(self.temporary_policies(), [])
        self.assertIsNotNone(self.cluster.get("Namespace", "hibana"))

    def test_unavailable_installation_can_be_removed_and_reinstalled(self):
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.output(self.cluster.install)
        pvc = resource("PersistentVolumeClaim", "hibana-data")
        self.cluster.live[resource_id(pvc)] = pvc
        record = self.cluster.record()
        record["resources"].append({"kind": "PersistentVolumeClaim", "name": "hibana-data"})
        self.cluster.save(record)
        self.output(self.cluster.uninstall)
        self.assertIsNone(self.cluster.get("ConfigMap", MARKER))
        self.assertIsNotNone(self.cluster.get("Namespace", "hibana"))
        self.assertIsNotNone(self.cluster.get("PersistentVolumeClaim", "hibana-data"))
        removed = [e[1] for e in self.cluster.events if e[0] == "kubectl" and "delete" in e[1]]
        cp = next(i for i, args in enumerate(removed) if "hibana-control-plane" in args and "Deployment" in args)
        worker = next(i for i, args in enumerate(removed) if "hibana-worker" in args and "Deployment" in args)
        self.assertLess(cp, worker)
        self.assertIn("--cascade=foreground", removed[cp])
        self.cluster.fail_rollout = False
        self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["install"]["status"], "complete")

    def test_missing_cp_does_not_poison_stop_state(self):
        self.output(self.cluster.install)
        before = self.cluster.record()
        with self.assertRaisesRegex(ValueError, "No running Control Plane"):
            self.cluster.stop()
        self.assertEqual(self.cluster.record(), before)
        self.output(self.cluster.install)

    def test_incomplete_stop_can_be_repaired_without_losing_gate_owner(self):
        self.output(self.cluster.install)
        record = self.cluster.record()
        record["paused"] = {"owner": "repair", "drained": False,
            "replicas": {"hibana-control-plane": 2, "hibana-worker": 2}, "hpas": []}
        self.cluster.save(record)
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.output(self.cluster.install)
        self.assertEqual(self.cluster.record()["paused"]["owner"], "repair")
        self.cluster.fail_rollout = False
        with patch("existing.Maintenance") as maintenance:
            maintenance.return_value.prepare.side_effect = ValueError("not prepared")
            with self.assertRaisesRegex(ValueError, "not prepared"):
                self.output(self.cluster.install)
            maintenance.return_value.open.assert_not_called()
            self.assertEqual(self.cluster.record()["paused"]["owner"], "repair")
            maintenance.return_value.prepare.side_effect = None
            self.output(self.cluster.install)
            maintenance.return_value.close.assert_called_with("repair")
            maintenance.return_value.open.assert_called_once_with("repair")
        self.assertNotIn("paused", self.cluster.record())
        self.assertEqual(self.cluster.record()["install"]["status"], "complete")

    def test_uninstall_does_not_bypass_a_failed_drain(self):
        self.output(self.cluster.install)
        self.cluster.events.clear()
        with patch("existing.Maintenance") as maintenance:
            maintenance.return_value.drain.side_effect = ValueError("still executing")
            with self.assertRaisesRegex(ValueError, "still executing"):
                self.cluster.uninstall()
        self.assertFalse(any("delete" in e[1] or "scale" in e[1]
            for e in self.cluster.events if e[0] == "kubectl"))
        self.assertIsNotNone(self.cluster.get("Deployment", "hibana-control-plane"))

    def test_failed_migration_records_phase_and_never_applies_deployments(self):
        self.cluster.fail_wait = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.cluster.install()
        installed = self.cluster.record()["install"]
        self.assertEqual(installed["status"], "failed")
        self.assertEqual(installed["phase"], "database migration")
        self.assertIn("configuration", installed["completed"])
        applied = [doc for event in self.cluster.events if event[0] == "mutation" for doc in event[1]]
        self.assertFalse(any(doc["kind"] == "Deployment" for doc in applied))

    def test_partial_rollout_can_be_retried_to_completion(self):
        self.cluster.fail_rollout = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.cluster.install()
        self.assertEqual(self.cluster.record()["install"]["phase"], "workload rollout")
        self.cluster.fail_rollout = False
        output = self.output(self.cluster.install)
        self.assertIn("installation complete", output)
        self.assertEqual(self.cluster.record()["install"]["status"], "complete")
        self.assertEqual(self.cluster.record()["install"]["completed"][-1], "management API check")
        self.assertNotIn("private-credential", json.dumps(self.cluster.record()))

    def test_fresh_install_performs_server_validation_before_configuration(self):
        self.cluster.fail_server = "Deployment"
        with self.assertRaisesRegex(ValueError, "rejected Deployment"):
            self.cluster.install()
        mutations = [doc for event in self.cluster.events if event[0] == "mutation" for doc in event[1]]
        self.assertEqual([doc["kind"] for doc in mutations], ["Namespace"])

    def test_external_management_readiness_is_checked_before_success_guidance(self):
        ingress = resource("Ingress", "hibana-management", spec={"tls": [{"hosts": ["api.test"]}], "rules": [{"host": "api.test", "http": {
            "paths": [{"backend": {"service": {"name": "hibana-api"}}}]}}]})
        with patch("existing.urllib.request.urlopen") as request:
            request.return_value.__enter__.return_value.status = 200
            request.return_value.__enter__.return_value.geturl.return_value = "https://api.test/readyz"
            request.return_value.__enter__.return_value.read.return_value = b'{"db":"ok","store":"ok"}'
            output = self.output(lambda: self.cluster.connection_guidance([ingress], verify=True))
        request.assert_called_once_with("https://api.test/readyz", timeout=5)
        self.assertIn("readiness verified", output)
        self.assertIn("hibana login --profile onprem", output)
        with patch("existing.urllib.request.urlopen", side_effect=OSError("certificate")), patch("existing.time.monotonic", side_effect=[0, 31]):
            with self.assertRaisesRegex(ValueError, "Check DNS, TLS, Ingress"):
                self.cluster.connection_guidance([ingress], verify=True)
        with patch("existing.urllib.request.urlopen") as request, patch("existing.time.monotonic", side_effect=[0, 31]):
            response = request.return_value.__enter__.return_value
            response.status, response.geturl.return_value = 200, "https://api.test/readyz"
            response.read.return_value = b'{"login":"please sign in"}'
            with self.assertRaisesRegex(ValueError, "not reachable or ready"):
                self.cluster.connection_guidance([ingress], verify=True)


if __name__ == "__main__":
    unittest.main()
