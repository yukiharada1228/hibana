"""Kubernetes operator helpers; no local cluster, source checkout or Docker dependency."""
import base64
import hashlib
import json
import posixpath
import re
import subprocess

import yaml

MIGRATION_JOB = "hibana-migrate"
PLATFORM_CONTAINERS = {
    ("Deployment", "hibana-control-plane"): "control-plane",
    ("Deployment", "hibana-worker"): "worker",
    ("Job", MIGRATION_JOB): "migrate",
}


def management_api_prefixes(rule):
    """Public API prefixes for the Hibana services behind an Ingress rule."""
    prefixes = {"hibana-api": "", "hibana-console": "/api"}
    services = {path.get("backend", {}).get("service", {}).get("name")
                for path in rule.get("http", {}).get("paths", [])}
    return {prefixes[service] for service in services if service in prefixes}


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
            name = PLATFORM_CONTAINERS.get((doc.get("kind"), doc.get("metadata", {}).get("name")))
            if name is None:
                continue
            pod = doc.get("spec", {}).get("template", {}).get("spec", {})
            containers = [container for container in pod.get("containers", []) if container.get("name") == name]
            if len(containers) != 1:
                raise ValueError(f"{doc['kind']}/{doc['metadata']['name']} must contain exactly one {name} container")
            containers[0]["image"] = image
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


def volume_references(volume):
    """Secret/ConfigMap references in direct and projected volumes."""
    for field, kind, name in (("secret", "Secret", "secretName"), ("configMap", "ConfigMap", "name")):
        if field in volume:
            ref = volume[field]
            yield kind, ref[name], ref
    for source in volume.get("projected", {}).get("sources", []):
        for field, kind in (("secret", "Secret"), ("configMap", "ConfigMap")):
            if field in source:
                ref = source[field]
                yield kind, ref["name"], ref


def file_values(source, key=None):
    """Canonical base64 file bytes, including binary data and stringData overrides."""
    secret = source["kind"] == "Secret"
    encoded = source.get("data" if secret else "binaryData", {})
    strings = source.get("stringData" if secret else "data", {})
    keys = encoded.keys() | strings.keys() if key is None else [key]
    try:
        return {key: base64.b64encode(strings[key].encode() if key in strings else base64.b64decode(
                    encoded[key].replace("\r", "").replace("\n", ""), validate=True)).decode()
                for key in keys if key in strings or key in encoded}
    except (ValueError, UnicodeError) as error:
        raise ValueError(f"{source['kind']}/{source['metadata']['name']} has invalid file data") from error


def mounted_settings(pod, require):
    mounts = {}
    for container in pod.get("containers", []) + pod.get("initContainers", []):
        for mount in container.get("volumeMounts", []):
            mounts.setdefault(mount["name"], []).append(mount)
    settings = {}
    for volume in pod.get("volumes", []):
        if volume["name"] not in mounts:
            continue
        references = list(volume_references(volume))
        if not references:
            continue
        files = {}
        for kind, name, ref in references:
            optional = ref.get("optional", False)
            if ref.get("items"):
                for item in ref["items"]:
                    values = require(kind, name, item["key"], optional) or {}
                    if item["key"] in values:
                        files[posixpath.normpath(item["path"])] = values[item["key"]]
            else:
                files.update(require(kind, name, optional=optional) or {})
        selected = {}
        for mount in mounts[volume["name"]]:
            # subPathExpr can depend on Pod-specific downward API values. Keep
            # all projected files in that case; the expression is in the template.
            subpath = posixpath.normpath("" if mount.get("subPathExpr") else mount.get("subPath", ""))
            selected.update({path: value for path, value in files.items()
                             if subpath == "." or path == subpath or path.startswith(subpath + "/")})
        settings[volume["name"]] = selected
    return settings


def stamp_runtime_settings(docs, credentials):
    """Restart consumers when their environment or mounted configuration changes."""
    sources = {(doc["kind"], doc["metadata"]["name"]): doc
               for doc in docs + credentials if doc["kind"] in ("ConfigMap", "Secret")}

    def require(kind, name, key=None, optional=False, *, text=False):
        source = sources.get((kind, name))
        if source is None:
            if optional:
                return {}
            raise ValueError(f"Missing {kind}/{name} for runtime settings")
        if text:
            values = secret_values(source, key) if kind == "Secret" else source.get("data", {})
        else:
            values = file_values(source, key)
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
        mounted = mounted_settings(pod, require)
        if mounted:
            settings.append({"volumes": mounted})
        digest = hashlib.sha256(json.dumps(settings, sort_keys=True).encode()).hexdigest()
        template.setdefault("metadata", {}).setdefault("annotations", {})["hibana.local/settings-hash"] = digest
