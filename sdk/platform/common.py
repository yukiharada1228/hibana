"""Kubernetes operator helpers; no local cluster, source checkout or Docker dependency."""
import hashlib
import json
import subprocess

import yaml


def run(*args, capture=False, input=None, env=None, quiet=False):
    return subprocess.run([str(arg) for arg in args], check=True, text=True,
                          input=input, stdout=subprocess.PIPE if capture else None, env=env,
                          stderr=subprocess.DEVNULL if quiet else None).stdout


class KubernetesTarget:
    def run(self, *args, **kwargs):
        return run(*args, **kwargs)

    def kube(self, *args, **kwargs):
        return self.run(*self.kubectl, *args, **kwargs)

    def render(self, path, image):
        docs = list(yaml.safe_load_all(self.run("kubectl", "kustomize", path, capture=True)))
        for doc in docs:
            pod = doc.get("spec", {}).get("template", {}).get("spec", {})
            for container in pod.get("containers", []):
                if container.get("image") in ("hibana-platform:dev", "registry.example.com/hibana/platform:replace-with-release"):
                    container["image"] = image
        return docs

    def apply(self, docs):
        # Secrets travel over stdin, never argv, stdout, or a generated tracked manifest.
        self.kube("apply", "-f", "-", input=json.dumps({"apiVersion": "v1", "kind": "List", "items": docs}))


def stamp_runtime_settings(docs, credentials):
    """Restart only the Pods whose envFrom data changed, including config-only updates."""
    sources = {(doc["kind"], doc["metadata"]["name"]): doc.get("stringData", doc.get("data", {}))
               for doc in docs + credentials if doc["kind"] in ("ConfigMap", "Secret")}
    for doc in docs:
        if doc["kind"] != "Deployment":
            continue
        template = doc["spec"]["template"]
        settings = []
        for container in template["spec"]["containers"]:
            for ref in container.get("envFrom", []):
                for key, kind in [("configMapRef", "ConfigMap"), ("secretRef", "Secret")]:
                    if key in ref:
                        settings.append(sources[kind, ref[key]["name"]])
        digest = hashlib.sha256(json.dumps(settings, sort_keys=True).encode()).hexdigest()
        template.setdefault("metadata", {}).setdefault("annotations", {})["hibana.local/settings-hash"] = digest
