"""Explicit-context Kubernetes installation; external storage is never removed."""
import json
from pathlib import Path

from common import KubernetesTarget, stamp_runtime_settings

MARKER = "hibana-platform"
LABEL = "app.kubernetes.io/managed-by"
WORKLOADS = ("hibana-control-plane", "hibana-worker")
KINDS = {"Namespace", "ConfigMap", "Secret", "ServiceAccount", "Service", "Deployment",
         "Job", "NetworkPolicy", "PodDisruptionBudget", "HorizontalPodAutoscaler", "Ingress",
         "PersistentVolumeClaim"}


def validate(docs):
    for doc in docs:
        kind, meta = doc.get("kind"), doc.get("metadata", {})
        name = meta.get("name", "")
        if kind not in KINDS or not (name.startswith("hibana-") or name == "hibana" or (kind, name) == ("NetworkPolicy", "default-deny")):
            raise ValueError(f"Unsupported installation resource: {kind}/{name}")
        if kind == "Namespace":
            if name != "hibana":
                raise ValueError("Only namespace hibana can be installed")
        elif meta.get("namespace") != "hibana":
            raise ValueError(f"Resource {kind}/{name} must explicitly use namespace hibana")
        if name == MARKER:
            raise ValueError(f"{MARKER} is reserved for installation state")


class ExistingCluster(KubernetesTarget):
    def __init__(self, kubeconfig, context, overlay=None, image=None):
        if not kubeconfig or not context or not Path(kubeconfig).is_file():
            raise ValueError("An existing --kubeconfig file and explicit --context are required")
        self.kubectl = ["kubectl", "--kubeconfig", str(kubeconfig), "--context", context]
        self.overlay, self.image = overlay, image

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

    def install(self):
        if not self.overlay or not self.image:
            raise ValueError("Installation requires --overlay and --image")
        docs = self.render(self.overlay, self.image)
        migration_path = Path(self.overlay) / "migration"
        if not migration_path.is_dir():
            migration_path = Path(__file__).resolve().parent / "manifests/migration"
        if not migration_path.is_dir():
            raise ValueError("Provide a migration/ Kustomize overlay, or use the packaged CLI (npm pack includes the default migration)")
        migration = self.render(migration_path, self.image)
        validate(docs + migration)
        namespace = self.get("namespace", "hibana")
        if namespace and namespace["metadata"].get("labels", {}).get("app.kubernetes.io/part-of") != "hibana":
            raise ValueError("Existing namespace hibana is not labelled as part of Hibana")
        record = self.record(required=False)
        if record.get("paused"):
            raise ValueError("Platform is stopped; run platform start before installing an update")
        for doc in docs + migration:
            current = self.get(doc["kind"], doc["metadata"]["name"])
            if current:
                labels = current["metadata"].get("labels", {})
                # Legacy releases used kubectl apply in the labelled Hibana
                # namespace without top-level resource labels.
                previous = json.loads(current["metadata"].get("annotations", {}).get("kubectl.kubernetes.io/last-applied-configuration", "{}"))
                legacy = namespace is not None and previous.get("metadata", {}).get("name") == doc["metadata"]["name"] and previous.get("metadata", {}).get("namespace") == "hibana"
                if labels.get(LABEL) not in (None, "hibana") or (labels.get(LABEL) != "hibana" and labels.get("app.kubernetes.io/part-of") != "hibana" and not legacy):
                    raise ValueError(f"Resource belongs to another installation: {doc['kind']}/{doc['metadata']['name']}")
        sources = list(docs)
        known = {(d["kind"], d["metadata"]["name"]) for d in sources}
        for doc in docs:
            for container in doc.get("spec", {}).get("template", {}).get("spec", {}).get("containers", []):
                for ref in container.get("envFrom", []):
                    for key, kind in (("configMapRef", "ConfigMap"), ("secretRef", "Secret")):
                        if key in ref and (kind, ref[key]["name"]) not in known:
                            source = self.get(kind, ref[key]["name"])
                            if not source:
                                raise ValueError(f"Missing external {kind}/{ref[key]['name']}")
                            sources.append(source)
                            known.add((kind, ref[key]["name"]))
        stamp_runtime_settings(docs, sources)
        for doc in docs + migration:
            doc["metadata"].setdefault("labels", {})[LABEL] = "hibana"
        self.apply([d for d in docs if d["kind"] == "Namespace"])
        inventory = {(r["kind"], r["name"]) for r in record["resources"]}
        inventory.update((d["kind"], d["metadata"]["name"]) for d in docs + migration if d["kind"] != "Namespace")
        record["resources"] = [{"kind": k, "name": n} for k, n in sorted(inventory)]
        self.save(record)
        self.apply([d for d in docs if d["kind"] in {"ConfigMap", "Secret", "ServiceAccount"}])
        current = self.get("job", "hibana-migrate")
        if current:
            if current.get("status", {}).get("succeeded", 0) != 1:
                raise ValueError("Migration has not succeeded; its Job was retained for inspection")
            self.kube("-n", "hibana", "delete", "job", "hibana-migrate")
        self.apply(migration)
        self.kube("-n", "hibana", "wait", "--for=condition=complete", "job/hibana-migrate", "--timeout=600s")
        self.apply(docs)
        self.ready()
        print("Hibana installed in namespace hibana. External dependencies were preserved.")

    def ready(self):
        for name in WORKLOADS:
            self.kube("-n", "hibana", "rollout", "status", f"deployment/{name}", "--timeout=300s")

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
            record["paused"] = {"replicas": {n: d["spec"].get("replicas", 1) for n, d in deployments.items()},
                                "hpas": [{"apiVersion": h["apiVersion"], "kind": h["kind"], "metadata": {"name": h["metadata"]["name"], "namespace": "hibana", "labels": h["metadata"].get("labels", {})}, "spec": h["spec"]} for h in hpas]}
            self.save(record)
        for hpa in record["paused"]["hpas"]:
            self.kube("-n", "hibana", "delete", "hpa", hpa["metadata"]["name"], "--ignore-not-found")
        for name in WORKLOADS:
            self.kube("-n", "hibana", "scale", f"deployment/{name}", "--replicas=0")
            self.kube("-n", "hibana", "wait", "--for=delete", "pod", "-l", f"app.kubernetes.io/name={name}", "--timeout=180s")
        print("Hibana stopped. Replicas, autoscaling settings and data are preserved.")

    def start(self):
        record = self.record()
        if record.get("paused"):
            for name, replicas in record["paused"]["replicas"].items():
                self.kube("-n", "hibana", "scale", f"deployment/{name}", f"--replicas={replicas}")
            if record["paused"]["hpas"]:
                self.apply(record["paused"]["hpas"])
            self.ready()
            del record["paused"]
            self.save(record)
        else:
            self.ready()
        print("Hibana started.")

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
        for resource in sorted(resources, key=lambda r: r["kind"] not in {"HorizontalPodAutoscaler", "Deployment", "Job"}):
            self.kube("-n", "hibana", "delete", resource["kind"], resource["name"], "--ignore-not-found", "--timeout=180s")
        self.kube("-n", "hibana", "delete", "configmap", MARKER)
        print("Hibana uninstalled. Cluster, namespace, PVCs and external dependencies were retained.")

    def status(self):
        record = self.record()
        print("Hibana: stopped" if record.get("paused") else "Hibana: installed")
        self.kube("-n", "hibana", "get", "deployments,pods,services,hpa", "-o", "wide")
