"""Explicit-context Kubernetes installation; external storage is never removed."""
import json
from contextlib import contextmanager
import shlex
import subprocess
import time
import threading
import urllib.error
import urllib.request
import uuid
from pathlib import Path

from common import KubernetesTarget, MIGRATION_JOB, stamp_runtime_settings
from maintenance import Maintenance, NoControlPlane
from network import NetworkTransition, PREFIX as NETWORK_PREFIX
from operation import OperationLock, NAME as OPERATION_LOCK, exclusive
from preflight import Preflight, resource_id
from readiness import Readiness

MARKER = "hibana-platform"
LABEL = "app.kubernetes.io/managed-by"
RESUME_OWNER = "hibana.io/maintenance-owner"
WORKLOADS = ("hibana-control-plane", "hibana-worker")
KINDS = {"Namespace", "ConfigMap", "Secret", "ServiceAccount", "Service", "Deployment",
         "Job", "NetworkPolicy", "PodDisruptionBudget", "HorizontalPodAutoscaler", "Ingress",
         "PersistentVolumeClaim"}


def validate(docs):
    for doc in docs:
        if not isinstance(doc, dict) or not isinstance(doc.get("metadata"), dict):
            raise ValueError("Each manifest must be a Kubernetes resource with metadata")
        kind, meta = doc.get("kind"), doc.get("metadata", {})
        name = meta.get("name", "")
        if kind not in KINDS or not (name.startswith("hibana-") or name == "hibana" or (kind, name) == ("NetworkPolicy", "default-deny")):
            raise ValueError(f"Unsupported installation resource: {kind}/{name}")
        if kind == "Namespace":
            if name != "hibana":
                raise ValueError("Only namespace hibana can be installed")
        elif meta.get("namespace") != "hibana":
            raise ValueError(f"Resource {kind}/{name} must explicitly use namespace hibana")
        if name in (MARKER, OPERATION_LOCK):
            raise ValueError(f"{name} is reserved for installation state")


class ExistingCluster(KubernetesTarget):
    def __init__(self, kubeconfig, context, overlay=None, image=None):
        if not kubeconfig or not context or not Path(kubeconfig).is_file():
            raise ValueError("An existing --kubeconfig file and explicit --context are required")
        self.kubectl = ["kubectl", "--kubeconfig", str(kubeconfig), "--context", context]
        self.overlay, self.image = overlay, image
        self.install_phase = "preflight"
        self._operation_mutex = threading.RLock()
        self._operation_active = False

    @contextmanager
    def operation(self, action):
        # uninstall calls stop under the same lock. The local mutex prevents a
        # second thread from accidentally treating another thread as nested.
        with self._operation_mutex:
            if self._operation_active:
                yield
                return
            if action == "install":
                print("Checking configuration, cluster access and permissions...")
                docs, _, namespace, *_ = self.prepare_install()
            else:
                namespace = self.get("namespace", "hibana")
            if not namespace and action == "install":
                # Validate before the first mutation. Atomic Namespace creation
                # bootstraps the lock's scope; all installation state is read
                # again under the lock, including after a concurrent creation.
                namespace_doc = next(d for d in docs if d["kind"] == "Namespace")
                try:
                    self.kube("create", "-f", "-", input=json.dumps(namespace_doc),
                              capture=True, quiet=True, timeout=30)
                except subprocess.CalledProcessError:
                    if not self.get("namespace", "hibana"):
                        raise
                namespace = self.get("namespace", "hibana")
            if not namespace:
                raise ValueError("No Hibana namespace; run platform install first")
            if namespace["metadata"].get("labels", {}).get("app.kubernetes.io/part-of") != "hibana":
                raise ValueError("Existing namespace hibana is not labelled as part of Hibana")
            with OperationLock(self, action):
                self._operation_active = True
                try:
                    yield
                finally:
                    self._operation_active = False

    def get(self, kind, name):
        raw = self.kube("-n", "hibana", "get", kind, name, "--ignore-not-found", "-o", "json", capture=True)
        return json.loads(raw) if raw.strip() else None

    def record(self, required=True):
        marker = self.get("configmap", MARKER)
        if marker and marker["metadata"].get("labels", {}).get(LABEL) != "hibana":
            raise ValueError("Installation record belongs to another manager")
        if not marker:
            if required:
                raise ValueError("No Hibana installation record; run platform install first")
            return {"resources": []}
        state = json.loads(marker["data"]["state.json"])
        if not isinstance(state.get("resources"), list):
            raise ValueError("Invalid installation inventory")
        for resource in state["resources"]:
            if resource.get("kind") == "Namespace":
                raise ValueError("Namespace deletion is outside the installation inventory")
            validate([{"kind": resource.get("kind"), "metadata": {"name": resource.get("name", ""), "namespace": "hibana"}}])
        paused = state.get("paused")
        if paused:
            if "owner" in paused and (not isinstance(paused["owner"], str) or not paused["owner"].isalnum() or len(paused["owner"]) > 128):
                raise ValueError("Invalid maintenance owner")
            if "drained" in paused and type(paused["drained"]) is not bool:
                raise ValueError("Invalid saved drain state")
            if set(paused.get("replicas", {})) != set(WORKLOADS) or any(type(n) is not int or n < 0 for n in paused["replicas"].values()):
                raise ValueError("Invalid saved runtime replicas")
            for hpa in paused.get("hpas", []):
                validate([hpa])
                target = hpa.get("spec", {}).get("scaleTargetRef", {})
                if hpa["kind"] != "HorizontalPodAutoscaler" or target.get("kind") != "Deployment" or target.get("name") not in WORKLOADS:
                    raise ValueError("Saved HPA does not target the Hibana runtime")
        return state

    def save(self, record):
        self.apply([{"apiVersion": "v1", "kind": "ConfigMap", "metadata": {
            "name": MARKER, "namespace": "hibana", "labels": {LABEL: "hibana"}},
            "data": {"state.json": json.dumps(record)}}])

    def prepare_install(self):
        if not self.overlay or not self.image:
            raise ValueError("Installation requires --overlay and --image")
        if not Path(self.overlay).is_dir():
            raise ValueError("The overlay directory does not exist. Create one with hibana platform init DIRECTORY.")
        if any(value in self.image for value in ("CHANGE_ME", "replace-with-release")):
            raise ValueError("Supply a published platform image with --image.")
        checks = Preflight(self)
        checks.connect()
        docs = self.render(self.overlay, self.image)
        migration_path = Path(self.overlay) / "migration"
        if not migration_path.is_dir():
            migration_path = Path(__file__).resolve().parent / "manifests/migration"
        if not migration_path.is_dir():
            raise ValueError("Provide a migration/ Kustomize overlay, or use the packaged CLI (npm pack includes the default migration)")
        migration = self.render(migration_path, self.image)
        validate(docs + migration)
        if any(d["kind"] == "NetworkPolicy" and d["metadata"]["name"].startswith(NETWORK_PREFIX) for d in docs + migration):
            raise ValueError(f"NetworkPolicy names starting with {NETWORK_PREFIX} are reserved for installation recovery")
        namespace = self.get("namespace", "hibana")
        if not namespace and not any(d["kind"] == "Namespace" for d in docs):
            raise ValueError("Namespace hibana does not exist. Include its Namespace manifest in the overlay.")
        if namespace and namespace["metadata"].get("labels", {}).get("app.kubernetes.io/part-of") != "hibana":
            raise ValueError("Existing namespace hibana is not labelled as part of Hibana")
        record = self.record(required=False)
        paused = record.get("paused")
        if paused and (paused.get("drained", True) or not paused.get("owner")):
            raise ValueError("Platform is stopped; run platform start before installing an update")
        current_resources = {}
        for doc in docs + migration:
            current = self.get(doc["kind"], doc["metadata"]["name"])
            current_resources[resource_id(doc)] = current
            if current:
                labels = current["metadata"].get("labels", {})
                # Legacy releases used kubectl apply in the labelled Hibana
                # namespace without top-level resource labels.
                previous = json.loads(current["metadata"].get("annotations", {}).get("kubectl.kubernetes.io/last-applied-configuration", "{}"))
                legacy = namespace is not None and previous.get("metadata", {}).get("name") == doc["metadata"]["name"] and previous.get("metadata", {}).get("namespace") == "hibana"
                if labels.get(LABEL) not in (None, "hibana") or (labels.get(LABEL) != "hibana" and labels.get("app.kubernetes.io/part-of") != "hibana" and not legacy):
                    raise ValueError(f"Resource belongs to another installation: {doc['kind']}/{doc['metadata']['name']}")
        current_job = current_resources.get(("Job", MIGRATION_JOB))
        if current_job and current_job.get("status", {}).get("succeeded", 0) != 1:
            raise ValueError("Migration has not succeeded; its Job was retained for inspection. Inspect job/hibana-migrate before retrying.")
        sources = checks.settings(docs + migration)
        # Pre-provisioned namespaces and Secrets are common on-prem. Namespace
        # existence alone must not delay isolation of the first runtime Pods.
        preserve_access = any(value for (kind, _), value in current_resources.items() if kind in {"Deployment", "Job"})
        if namespace and not preserve_access:
            preserve_access = bool(json.loads(self.kube("-n", "hibana", "get", "pods", "-o", "json",
                                                       capture=True, quiet=True, timeout=20))["items"])
        network = NetworkTransition(self, docs + migration, record, bool(namespace), preserve_access=preserve_access)
        current_resources.update({("NetworkPolicy", n): d for n, d in network.current.items() if n in network.cleanup})
        checks.permissions(docs + migration + network.temporary, current_resources, delete_policies=network.cleanup)
        stamp_runtime_settings(docs, sources)
        for doc in docs + migration:
            doc["metadata"].setdefault("labels", {})[LABEL] = "hibana"
        print("Installation plan:")
        print(f"Platform image: {self.image}")
        checks.preview(docs + migration, current_resources, bool(namespace))
        if network.temporary or network.cleanup:
            print("Temporary network access before migration (existing access retained until old Pods exit):")
            checks.preview(network.temporary, current_resources, bool(namespace))
            for name in sorted(network.cleanup):
                print(f"  delete    NetworkPolicy/{name} (after old Pods exit and final policies)")
        desired = {resource_id(d) for d in docs + migration} | {("NetworkPolicy", n) for n in network.cleanup}
        for item in record["resources"]:
            if (item["kind"], item["name"]) not in desired:
                print(f"  retain    {item['kind']}/{item['name']} (not in this overlay)")
        return docs, migration, namespace, record, current_job, network

    @exclusive
    def install(self):
        print("Revalidating installation while holding the platform operation lock...")
        docs, migration, namespace, record, current_job, network = self.prepare_install()
        record["install"] = {"image": self.image, "phase": "namespace", "status": "running", "completed": []}
        saved = False
        def phase(name):
            self.install_phase = name
            record["install"]["phase"] = name
            if saved:
                self.save(record)
            print(f"Installing: {name}")
        try:
            phase("namespace")
            namespace_docs = [d for d in docs if d["kind"] == "Namespace"]
            if namespace_docs:
                self.apply(namespace_docs)
            record["install"]["completed"].append("namespace")
            # prepare_install revalidated every namespaced object after the
            # operation lock's namespace was created and its lock acquired.
            inventory = {(r["kind"], r["name"]) for r in record["resources"]}
            inventory.update(resource_id(d) for d in docs + migration if d["kind"] != "Namespace")
            inventory.update(("NetworkPolicy", n) for n in network.cleanup)
            record["resources"] = [{"kind": k, "name": n} for k, n in sorted(inventory)]
            self.save(record)
            saved = True
            phase("configuration")
            self.apply([d for d in docs if d["kind"] in {"ConfigMap", "Secret", "ServiceAccount"}])
            record["install"]["completed"].append("configuration")
            phase("network access")
            if network.before_migration:
                self.apply(network.before_migration)
            record["install"]["completed"].append("network access")
            phase("database migration")
            if current_job:
                self.kube("-n", "hibana", "delete", "job", MIGRATION_JOB)
            self.apply([d for d in migration if d["kind"] != "NetworkPolicy"])
            self.kube("-n", "hibana", "wait", "--for=condition=complete", f"job/{MIGRATION_JOB}", "--timeout=600s")
            record["install"]["completed"].append("database migration")
            phase("workload rollout")
            self.apply([d for d in docs if d["kind"] != "NetworkPolicy"])
            self.ready({d["metadata"]["name"] for d in docs if d["kind"] == "Deployment"} | set(WORKLOADS))
            record["install"]["completed"].append("workload rollout")
            phase("old Pod termination")
            Readiness(self).wait_termination()
            record["install"]["completed"].append("old Pod termination")
            phase("network policy reconciliation")
            network.finish()
            record["resources"] = [r for r in record["resources"]
                                   if not (r["kind"] == "NetworkPolicy" and r["name"] in network.cleanup)]
            record["install"]["completed"].append("network policy reconciliation")
            phase("dependency verification")
            self.verify_dependencies()
            record["install"]["completed"].append("dependency verification")
            namespace_owner = (namespace or {}).get("metadata", {}).get("annotations", {}).get(RESUME_OWNER)
            paused = record.get("paused", {})
            owner = paused.get("owner") or namespace_owner
            if owner:
                phase("application readiness")
                maintenance = Maintenance(self)
                maintenance.close(owner)
                maintenance.prepare(owner)
                desired = {resource_id(d) for d in docs}
                restore_hpas = [h for h in paused.get("hpas", []) if resource_id(h) not in desired]
                if restore_hpas:
                    self.apply(restore_hpas)
                maintenance.open(owner)
                record.pop("paused", None)
                record["install"]["completed"].append("application readiness")
            if namespace_owner:
                self.kube("annotate", "namespace", "hibana", RESUME_OWNER + "-")
            phase("management API check")
            self.connection_guidance(docs, verify=True)
            record["install"]["completed"].append("management API check")
            record["install"].update(phase="complete", status="complete")
            self.save(record)
        except (ValueError, OSError, subprocess.SubprocessError, KeyboardInterrupt):
            if saved:
                record["install"]["status"] = "failed"
                try:
                    self.save(record)
                except (ValueError, OSError, subprocess.SubprocessError):
                    pass
            raise
        print("Hibana installation complete.")

    def preview(self, action):
        if action == "install":
            self.prepare_install()
        elif action == "status":
            self.status()
        else:
            Preflight(self).connect()
            record = self.record()
            if action == "uninstall":
                for resource in record["resources"]:
                    kind, name = resource["kind"], resource["name"]
                    current = self.get(kind, name)
                    if current and current["metadata"].get("labels", {}).get(LABEL) != "hibana":
                        raise ValueError(f"Resource ownership changed: {kind}/{name}")
                    print(f"  {'retain' if kind == 'PersistentVolumeClaim' else 'delete' if current else 'absent':9} {kind}/{name}")
                print("  retain    cluster, namespace and external dependencies")
            elif action == "stop":
                for name in WORKLOADS:
                    current = self.get("deployment", name)
                    if not current or current["metadata"].get("labels", {}).get(LABEL) != "hibana":
                        raise ValueError(f"Deployment/{name} is not owned by this installation")
                    print(f"  scale     Deployment/{name}: {current['spec'].get('replicas', 1)} -> 0 (after draining)")
            elif action == "start":
                paused = record.get("paused")
                if paused:
                    for name, replicas in paused["replicas"].items():
                        print(f"  restore   Deployment/{name}: {replicas} replicas")
                    print(f"  restore   {len(paused['hpas'])} saved autoscalers, then verify application readiness")
                else:
                    print("  unchanged Platform is already started; readiness will be checked")
        print("Dry run complete. No resources were changed.")

    def connection_guidance(self, docs, verify=False):
        hosts = set()
        for doc in docs:
            if doc["kind"] != "Ingress":
                continue
            spec = doc.get("spec", {})
            tls_hosts = {host for tls in spec.get("tls", []) for host in tls.get("hosts", [])}
            for rule in spec.get("rules", []):
                host = rule.get("host", "")
                if host and "*" not in host and any(p.get("backend", {}).get("service", {}).get("name") == "hibana-api" for p in rule.get("http", {}).get("paths", [])):
                    hosts.add(("https://" if host in tls_hosts else "http://") + host)
        if not hosts:
            print("No management Ingress is configured. To connect locally:")
            print("  " + shlex.join([*self.kubectl, "-n", "hibana", "port-forward", "service/hibana-api", "18080:8080"]))
            print("  hibana login --url http://127.0.0.1:18080 --tenant TEAM --email EMAIL --password-stdin < password.txt")
            return
        for url in sorted(hosts):
            print(f"Management API: {url}")
            if verify:
                deadline = time.monotonic() + 30
                while True:
                    try:
                        with urllib.request.urlopen(url + "/readyz", timeout=5) as response:
                            if response.status != 200 or response.geturl() != url + "/readyz":
                                raise ValueError("Management API is not ready")
                            health = json.loads(response.read(8192))
                            if not isinstance(health, dict) or health.get("db") != "ok" or health.get("store") != "ok":
                                raise ValueError("Management API dependencies are not ready")
                        break
                    except (OSError, ValueError) as error:
                        if isinstance(error, urllib.error.HTTPError):
                            error.close()
                        if time.monotonic() >= deadline:
                            raise ValueError(f"Management API is not reachable or ready at {url}. Check DNS, TLS, Ingress and dependency health, then rerun install.") from None
                        time.sleep(1)
                print("Management API readiness verified from this computer.")
            print("Next (use an existing tenant account):")
            print(f"  hibana login --profile onprem --url {shlex.quote(url)} --tenant TEAM --email EMAIL --password-stdin < password.txt")
            print("  hibana deploy --profile onprem")

    def verify_dependencies(self):
        Readiness(self).verify_dependencies()

    def ready(self, deployments=WORKLOADS):
        for name in sorted(deployments):
            self.kube("-n", "hibana", "rollout", "status", f"deployment/{name}", "--timeout=300s")

    @exclusive
    def stop(self):
        record = self.record()
        if not record.get("paused"):
            deployments = {name: self.get("deployment", name) for name in WORKLOADS}
            if any(not d or d["metadata"].get("labels", {}).get(LABEL) != "hibana" for d in deployments.values()):
                raise ValueError("Runtime deployments are not owned by this installation")
            hpas = json.loads(self.kube("-n", "hibana", "get", "hpa", "-o", "json", capture=True))["items"]
            hpas = [h for h in hpas if h["spec"]["scaleTargetRef"].get("kind") == "Deployment" and h["spec"]["scaleTargetRef"]["name"] in WORKLOADS]
            if any(h["metadata"].get("labels", {}).get(LABEL) != "hibana" for h in hpas):
                raise ValueError("An external HPA controls Hibana; include it in the installation overlay before stopping")
            # No state mutation if the initial installation never started a CP.
            # Keep the owner durable before close itself: its reply may be lost.
            Maintenance(self).control_plane()
            record["paused"] = {"owner": uuid.uuid4().hex, "drained": False,
                                "replicas": {n: d["spec"].get("replicas", 1) for n, d in deployments.items()},
                                "hpas": [{"apiVersion": h["apiVersion"], "kind": h["kind"], "metadata": {"name": h["metadata"]["name"], "namespace": "hibana", "labels": h["metadata"].get("labels", {})}, "spec": h["spec"]} for h in hpas]}
            self.save(record)
        paused = record["paused"]
        if not paused.get("drained"):
            paused.setdefault("owner", uuid.uuid4().hex)
            self.save(record)
            maintenance = Maintenance(self)
            maintenance.close(paused["owner"])
        for hpa in paused["hpas"]:
            self.kube("-n", "hibana", "delete", "hpa", hpa["metadata"]["name"], "--ignore-not-found")
        if not paused.get("drained"):
            maintenance.drain()
            paused["drained"] = True
            self.save(record)
        for name in reversed(WORKLOADS):
            self.kube("-n", "hibana", "scale", f"deployment/{name}", "--replicas=0")
            self.kube("-n", "hibana", "wait", "--for=delete", "pod", "-l", f"app.kubernetes.io/name={name}", "--timeout=180s")
        print("Hibana stopped. Replicas, autoscaling settings and data are preserved.")

    @exclusive
    def start(self):
        record = self.record()
        if record.get("paused"):
            record["paused"]["drained"] = False
            self.save(record)
            for name, replicas in record["paused"]["replicas"].items():
                self.kube("-n", "hibana", "scale", f"deployment/{name}", f"--replicas={replicas}")
            self.ready()
            if record["paused"].get("owner"):
                maintenance = Maintenance(self)
                maintenance.close(record["paused"]["owner"])
                maintenance.prepare(record["paused"]["owner"])
                if record["paused"]["hpas"]:
                    self.apply(record["paused"]["hpas"])
                maintenance.open(record["paused"]["owner"])
            elif record["paused"]["hpas"]:
                self.apply(record["paused"]["hpas"])
            del record["paused"]
            self.save(record)
        else:
            self.ready()
        print("Hibana started.")

    @exclusive
    def uninstall(self):
        record = self.record()
        resources = []
        for resource in record["resources"]:
            if resource["kind"] == "PersistentVolumeClaim":
                continue
            current = self.get(resource["kind"], resource["name"])
            if current and current["metadata"].get("labels", {}).get(LABEL) != "hibana":
                raise ValueError(f"Resource ownership changed: {resource['kind']}/{resource['name']}")
            if current:
                resources.append(resource)
        if all(any(r["kind"] == "Deployment" and r["name"] == name for r in resources) for name in WORKLOADS):
            try:
                self.stop()
            except NoControlPlane:
                # Uninstall also cleans failed installations. Do not bypass a
                # busy/failed drain or transport error from a running CP.
                print("No Control Plane is running. Removing the unavailable installation with Kubernetes termination grace periods.")
        stopped = self.record()
        if stopped.get("paused", {}).get("owner"):
            # External DB survives uninstall with its gate closed. Preserve the
            # owner on the retained namespace so a reinstall can safely resume it.
            self.kube("annotate", "namespace", "hibana", f"{RESUME_OWNER}={stopped['paused']['owner']}", "--overwrite")
        for resource in sorted(resources, key=lambda r: (0 if r["kind"] == "HorizontalPodAutoscaler" else
                1 if (r["kind"], r["name"]) == ("Deployment", WORKLOADS[0]) else
                2 if r["kind"] in {"Deployment", "Job"} else 3)):
            # Foreground deletion stops CP controllers and their Pods before
            # removing Workers, preventing a Pending CP from starting mid-cleanup.
            self.kube("-n", "hibana", "delete", resource["kind"], resource["name"], "--ignore-not-found", "--cascade=foreground", "--timeout=180s")
        self.kube("-n", "hibana", "delete", "configmap", MARKER)
        print("Hibana uninstalled. Cluster, namespace, PVCs and external dependencies were retained.")

    def status(self):
        record = self.record()
        print("Hibana: stopped" if record.get("paused") else "Hibana: last installation incomplete" if record.get("install", {}).get("status") == "failed" else "Hibana: installed")
        if record.get("install"):
            install = record["install"]
            print(f"Last installation: {install['status']} ({install['phase']})")
        self.kube("-n", "hibana", "get", "deployments,pods,services,hpa", "-o", "wide")
