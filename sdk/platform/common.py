"""Kubernetes operator helpers; no local cluster, source checkout or Docker dependency."""
import base64
import hashlib
import json
import re
import subprocess

import yaml

MIGRATION_JOB = "hibana-migrate"


def run(*args, capture=False, input=None, env=None, quiet=False, timeout=None):
    return subprocess.run([str(arg) for arg in args], check=True, text=True,
                          input=input, stdout=subprocess.PIPE if capture else None, env=env,
                          stderr=subprocess.DEVNULL if quiet else None, timeout=timeout).stdout


class KubernetesTarget:
    def run(self, *args, **kwargs):
        return run(*args, **kwargs)

    def kube(self, *args, **kwargs):
        return self.run(*self.kubectl, *args, **kwargs)

    def render(self, path, image):
        try:
            rendered = self.run("kubectl", "kustomize", path, capture=True, quiet=True)
        except subprocess.CalledProcessError as error:
            raise ValueError(f"Could not render overlay {path}. Check kustomization.yaml, referenced files and env-file syntax.") from error
        docs = [doc for doc in yaml.safe_load_all(rendered) if doc is not None]
        for doc in docs:
            pod = doc.get("spec", {}).get("template", {}).get("spec", {})
            for container in pod.get("containers", []):
                if container.get("image") in ("hibana-platform:dev", "registry.example.com/hibana/platform:replace-with-release"):
                    container["image"] = image
        return docs

    def apply(self, docs):
        # Secrets travel over stdin, never argv, stdout, or a generated tracked manifest.
        try:
            self.kube("apply", "-f", "-", input=json.dumps({"apiVersion": "v1", "kind": "List", "items": docs}), capture=True, quiet=True)
        except subprocess.CalledProcessError as error:
            resources = ", ".join(f"{doc['kind']}/{doc['metadata']['name']}" for doc in docs)
            raise ValueError(f"Kubernetes could not apply {resources}. Inspect cluster events and retry after fixing the issue.") from error


def secret_values(doc, key=None):
    """Decode only the data consumed as environment text, after stringData overrides."""
    encoded, strings = doc.get("data", {}), doc.get("stringData", {})
    keys = encoded.keys() | strings.keys() if key is None else [key]
    try:
        return {key: strings[key] if key in strings else base64.b64decode(
                    encoded[key].replace("\r", "").replace("\n", ""), validate=True).decode()
                for key in keys if key in strings or key in encoded}
    except (ValueError, UnicodeError) as error:
        raise ValueError(f"Secret/{doc['metadata']['name']} has invalid text data") from error


def environment_values(container, require):
    """Resolve env sources in Kubernetes order, sharing preflight and rollout semantics."""
    env = {}
    for item in container.get("envFrom", []):
        for key, kind in (("configMapRef", "ConfigMap"), ("secretRef", "Secret")):
            if key in item:
                ref = item[key]
                values = require(kind, ref["name"], optional=ref.get("optional", False), text=True) or {}
                env.update({item.get("prefix", "") + k: v for k, v in values.items()})
    for item in container.get("env", []):
        if "value" in item:
            # Explicit values expand preceding env/envFrom values; $$ escapes $.
            env[item["name"]] = re.sub(r"\$\$|\$\(([^)]+)\)",
                lambda m: "$" if m[0] == "$$" else str(env.get(m[1], m[0])), item["value"])
        source = item.get("valueFrom", {})
        for key, kind in (("configMapKeyRef", "ConfigMap"), ("secretKeyRef", "Secret")):
            if key in source:
                ref = source[key]
                values = require(kind, ref["name"], ref["key"], ref.get("optional", False), text=True) or {}
                # Kubernetes skips absent optional keys, retaining any envFrom value.
                if ref["key"] in values:
                    env[item["name"]] = values[ref["key"]]
        if "fieldRef" in source or "resourceFieldRef" in source:
            # The value is assigned by kubelet and does not depend on config data.
            env[item["name"]] = source
    return env


def stamp_runtime_settings(docs, credentials):
    """Restart Deployments when the environment of a container or init container changes."""
    sources = {(doc["kind"], doc["metadata"]["name"]): doc
               for doc in docs + credentials if doc["kind"] in ("ConfigMap", "Secret")}

    def require(kind, name, key=None, optional=False, *, text=False):
        source = sources.get((kind, name))
        if source is None:
            if optional:
                return {}
            raise ValueError(f"Missing {kind}/{name} for runtime settings")
        values = secret_values(source, key) if kind == "Secret" else source.get("data", {})
        if key is not None and key not in values and not optional:
            raise ValueError(f"Missing {kind}/{name} key {key}")
        return values

    for doc in docs:
        if doc["kind"] != "Deployment":
            continue
        template = doc["spec"]["template"]
        pod = template["spec"]
        settings = [environment_values(container, require)
                    for container in pod.get("containers", []) + pod.get("initContainers", [])]
        digest = hashlib.sha256(json.dumps(settings, sort_keys=True).encode()).hexdigest()
        template.setdefault("metadata", {}).setdefault("annotations", {})["hibana.local/settings-hash"] = digest
