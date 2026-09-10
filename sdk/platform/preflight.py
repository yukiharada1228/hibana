"""Read-only installation checks and value-free resource change previews."""
import base64
from copy import deepcopy
import json
import re
import shutil
import subprocess
from urllib.parse import urlsplit

from common import MIGRATION_JOB, environment_values, secret_values


RESOURCES = {
    "Namespace": "namespaces", "ConfigMap": "configmaps", "Secret": "secrets",
    "ServiceAccount": "serviceaccounts", "Service": "services", "Deployment": "deployments.apps",
    "Job": "jobs.batch", "NetworkPolicy": "networkpolicies.networking.k8s.io",
    "PodDisruptionBudget": "poddisruptionbudgets.policy", "HorizontalPodAutoscaler": "horizontalpodautoscalers.autoscaling",
    "Ingress": "ingresses.networking.k8s.io", "PersistentVolumeClaim": "persistentvolumeclaims",
}


def resource_id(doc):
    return doc["kind"], doc["metadata"]["name"]


def fields_changed(before, after, path=""):
    """Return JSON-pointer paths only. Never expose configuration or Secret values."""
    if isinstance(before, dict) and isinstance(after, dict):
        result = []
        for key in sorted(before.keys() | after.keys()):
            child = path + "/" + key.replace("~", "~0").replace("/", "~1")
            if key not in before or key not in after:
                result.append(child)
            else:
                result.extend(fields_changed(before[key], after[key], child))
        return result
    if isinstance(before, list) and isinstance(after, list) and len(before) == len(after):
        return [field for i, (left, right) in enumerate(zip(before, after))
                for field in fields_changed(left, right, f"{path}/{i}")]
    return [path or "/"] if before != after else []


def comparable(doc):
    doc = deepcopy(doc)
    doc.pop("status", None)
    meta = doc.get("metadata", {})
    for key in ("managedFields", "resourceVersion", "uid", "creationTimestamp", "generation"):
        meta.pop(key, None)
    meta.get("annotations", {}).pop("kubectl.kubernetes.io/last-applied-configuration", None)
    if not meta.get("annotations"):
        meta.pop("annotations", None)
    if doc.get("kind") == "Secret":
        data = doc.setdefault("data", {})
        # Kubernetes accepts CR/LF in base64; Kustomize wraps long generated values.
        data.update({key: value.replace("\r", "").replace("\n", "") for key, value in data.items()})
        data.update({k: base64.b64encode(v.encode()).decode() for k, v in doc.pop("stringData", {}).items()})
    return doc


def pod_spec(doc):
    return doc.get("spec", {}).get("template", {}).get("spec", {})


class Preflight:
    def __init__(self, target):
        self.target = target

    def connect(self):
        if not shutil.which("kubectl"):
            raise ValueError("kubectl was not found. Install it on this computer and ensure it is on PATH.")
        try:
            self.target.kube("get", "--raw", "/version", capture=True, quiet=True, timeout=20)
        except subprocess.SubprocessError as error:
            raise ValueError("Cannot reach the selected Kubernetes API. Check the kubeconfig, context, network and credentials.") from error

    def settings(self, docs):
        errors = []
        identities = [resource_id(doc) for doc in docs]
        if len(set(identities)) != len(identities):
            errors.append("The overlay contains duplicate resource identities")
        for doc in docs:
            label = "/".join(resource_id(doc))
            # Names and paths are sufficient to find unfinished template settings.
            def check(value, path=""):
                if isinstance(value, dict):
                    for key, item in value.items():
                        check(item, path + "/" + str(key))
                elif isinstance(value, list):
                    for index, item in enumerate(value):
                        check(item, f"{path}/{index}")
                elif isinstance(value, str) and re.search(r"CHANGE_ME|replace-with-release|example\.internal", value):
                    errors.append(f"{label}{path}: replace the example setting")
            check(doc)
        available = {resource_id(doc): doc for doc in docs}

        def require(kind, name, key=None, optional=False, *, text=False):
            ref = (kind, name)
            if ref not in available:
                found = self.target.get(kind, name)
                if found:
                    available[ref] = found
            source = available.get(ref)
            if source is None:
                if not optional:
                    errors.append(f"Missing {kind}/{name}; include it in the overlay or provision it in namespace hibana")
                return
            values = source.get("data", {})
            if kind == "Secret":
                values = secret_values(source, key) if text else {**values, **source.get("stringData", {})}
            if key is not None and key not in values and not optional:
                errors.append(f"Missing {kind}/{name} key {key}")
            return values

        for doc in docs:
            pod = pod_spec(doc)
            if pod.get("serviceAccountName"):
                require("ServiceAccount", pod["serviceAccountName"])
            for container in pod.get("containers", []) + pod.get("initContainers", []):
                for ref in container.get("envFrom", []):
                    for key, kind in (("configMapRef", "ConfigMap"), ("secretRef", "Secret")):
                        if key in ref:
                            require(kind, ref[key]["name"], optional=ref[key].get("optional", False), text=True)
                for env in container.get("env", []):
                    for key, kind in (("configMapKeyRef", "ConfigMap"), ("secretKeyRef", "Secret")):
                        ref = env.get("valueFrom", {}).get(key)
                        if ref:
                            require(kind, ref["name"], ref["key"], ref.get("optional", False), text=True)
            for ref in pod.get("imagePullSecrets", []):
                require("Secret", ref["name"])
            for volume in pod.get("volumes", []):
                if volume.get("persistentVolumeClaim"):
                    require("PersistentVolumeClaim", volume["persistentVolumeClaim"]["claimName"])
                for key, kind, name in (("secret", "Secret", "secretName"), ("configMap", "ConfigMap", "name")):
                    ref = volume.get(key)
                    if ref:
                        require(kind, ref[name], optional=ref.get("optional", False))
            if doc["kind"] == "Ingress":
                spec = doc.get("spec", {})
                if spec.get("ingressClassName"):
                    require("IngressClass", spec["ingressClassName"])
                tls_hosts = {host for tls in spec.get("tls", []) for host in tls.get("hosts", [])}
                for rule in spec.get("rules", []):
                    management = any(p.get("backend", {}).get("service", {}).get("name") == "hibana-api" for p in rule.get("http", {}).get("paths", []))
                    if management and rule.get("host") not in tls_hosts:
                        errors.append(f"Ingress/{doc['metadata']['name']}: configure TLS for the management hostname")
                for tls in doc.get("spec", {}).get("tls", []):
                    if tls.get("secretName"):
                        for key in ("tls.crt", "tls.key"):
                            values = require("Secret", tls["secretName"], key, text=True)
                            if values is not None and not str(values.get(key, "")).strip():
                                errors.append(f"Missing Secret/{tls['secretName']} key {key}")

        required = {"hibana-runtime": ["DATABASE_URL"], "hibana-control-plane": [
            "REDIS_URL", "S3_ACCESS_KEY", "S3_SECRET_KEY", "BOOTSTRAP_ADMIN_TOKEN", "JOB_SIGNING_KEY", "SECRETS_MASTER_KEY"],
            "hibana-migration": ["MIGRATION_DATABASE_URL"]}
        # Check the effective env rather than prescribing a Secret layout.
        for doc in docs:
            for container in pod_spec(doc).get("containers", []):
                name = container.get("name")
                if (doc["kind"], doc["metadata"]["name"], name) not in (
                    ("Deployment", "hibana-control-plane", "control-plane"),
                    ("Deployment", "hibana-worker", "worker"), ("Job", MIGRATION_JOB, "migrate")):
                    continue
                env = environment_values(container, require)
                keys = list(required["hibana-migration"] if name == "migrate" else required["hibana-runtime"])
                if name == "control-plane":
                    keys += required["hibana-control-plane"] + ["S3_ENDPOINT", "S3_BUCKET", "INGRESS_BASE_DOMAIN"]
                for key in keys:
                    if not str(env.get(key, "")).strip():
                        errors.append(f"{doc['kind']}/{doc['metadata']['name']}: missing environment setting {key}")
                schemes = {"DATABASE_URL": {"postgres", "postgresql"}, "MIGRATION_DATABASE_URL": {"postgres", "postgresql"},
                           "REDIS_URL": {"redis", "rediss"}, "S3_ENDPOINT": {"http", "https"}}
                for key, allowed in schemes.items():
                    if not env.get(key):
                        continue
                    try:
                        url = urlsplit(env[key])
                        valid = url.scheme in allowed and bool(url.hostname) and (url.port is None or 0 < url.port <= 65535)
                    except ValueError:
                        valid = False
                    if not valid:
                        errors.append(f"{doc['kind']}/{doc['metadata']['name']}: {key} must be a valid {'/'.join(sorted(allowed))} URL")
                for key, value in env.items():
                    if re.search(r"CHANGE_ME|replace-with-release|example\.internal", str(value)):
                        errors.append(f"{doc['kind']}/{doc['metadata']['name']}: replace example environment setting {key}")
        if errors:
            raise ValueError("Configuration checks failed:\n  " + "\n  ".join(dict.fromkeys(errors)))
        return list(available.values())

    def permissions(self, docs, current, *, delete_policies=()):
        checks = set()
        for doc in docs:
            kind, name = resource_id(doc)
            scope = () if kind == "Namespace" else ("-n", "hibana")
            resource = RESOURCES[kind]
            verbs = {"get", "patch"}
            if not current.get((kind, name)) or (kind, name) == ("Job", MIGRATION_JOB):
                verbs.add("create")
            for verb in verbs:
                checks.add((verb, resource if verb == "create" else f"{resource}/{name}", scope))
            if (kind, name) == ("Job", MIGRATION_JOB) and current.get((kind, name)):
                checks.add(("delete", f"{resource}/{name}", scope))
        for verb in ("get", "create", "patch"):
            checks.add((verb, "configmaps" if verb == "create" else "configmaps/hibana-platform", ("-n", "hibana")))
        for verb in ("get", "create", "update"):
            checks.add((verb, "configmaps" if verb == "create" else "configmaps/hibana-platform-operation", ("-n", "hibana")))
        for resource in ("deployments.apps", "jobs.batch", "pods"):
            for verb in ("get", "list", "watch"):
                checks.add((verb, resource, ("-n", "hibana")))
        for verb in ("get", "create"):
            checks.add((verb, "pods/exec", ("-n", "hibana")))
        if delete_policies or any(d["kind"] == "NetworkPolicy" for d in docs):
            checks.add(("list", RESOURCES["NetworkPolicy"], ("-n", "hibana")))
        for name in delete_policies:
            for verb in ("get", "delete"):
                checks.add((verb, f"{RESOURCES['NetworkPolicy']}/{name}", ("-n", "hibana")))
        denied = []
        for verb, resource, scope in sorted(checks):
            try:
                answer = self.target.kube(*scope, "auth", "can-i", verb, resource, capture=True, quiet=True, timeout=20)
                if answer.strip() != "yes":
                    denied.append(f"{verb} {resource}")
            except subprocess.CalledProcessError:
                denied.append(f"{verb} {resource}")
        if denied:
            raise ValueError("Kubernetes permissions are missing:\n  " + "\n  ".join(denied))

    def server_check(self, doc, recreate=False):
        candidate = deepcopy(doc)
        if recreate:
            candidate["metadata"].pop("name")
            candidate["metadata"]["generateName"] = "hibana-migrate-check-"
        try:
            raw = self.target.kube("create" if recreate else "apply", "--dry-run=server", "-f", "-", "-o", "json",
                input=json.dumps(candidate), capture=True, quiet=True, timeout=30)
            return json.loads(raw)
        except subprocess.CalledProcessError as error:
            raise ValueError(f"Kubernetes rejected {'/'.join(resource_id(doc))} during server validation. Check its schema, admission policies and quota.") from error

    def preview(self, docs, current, namespace_exists):
        pending = False
        for doc in docs:
            key = resource_id(doc)
            before = current.get(key)
            recreate = key == ("Job", MIGRATION_JOB) and bool(before)
            if namespace_exists or doc["kind"] == "Namespace":
                after = self.server_check(doc, recreate=recreate)
            else:
                after = doc
                pending = True
            action = "create" if not before else "recreate" if recreate else "update"
            changed = fields_changed(comparable(before), comparable(after)) if before else []
            if before and not changed and action != "recreate":
                action = "unchanged"
            print(f"  {action:9} {'/'.join(key)}")
            if changed:
                print("            fields: " + ", ".join(changed[:12]) + (", ..." if len(changed) > 12 else ""))
        if pending:
            print("Namespace hibana does not exist. Namespaced server validation will run after namespace creation during install.")
        print("Secret and configuration values are omitted from this preview.")
