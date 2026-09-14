#!/usr/bin/env python3
"""Manage the checkout-owned kind environment. Kubernetes resources live in Kustomize."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

import yaml
from common import KubernetesTarget, stamp_runtime_settings
from maintenance import Maintenance, NoControlPlane

ROOT = Path(__file__).resolve().parents[2]
LOCAL = ROOT / "deploy/kubernetes/local"


def run(*args, capture=False, input=None, env=None, quiet=False, timeout=None):
    return subprocess.run(
        [str(arg) for arg in args], cwd=ROOT, check=True, text=True,
        input=input, stdout=subprocess.PIPE if capture else None, env=env,
        stderr=subprocess.DEVNULL if quiet else None,
        timeout=timeout,
    ).stdout


class LocalCluster(KubernetesTarget):
    def __init__(self, name="hibana"):
        if not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name):
            raise ValueError("Cluster name must contain lowercase letters, digits and hyphens.")
        self.name = name
        self.state = ROOT / ".local" / ("kubernetes" if name == "hibana-dev" else f"kubernetes-{name}")
        self.kubeconfig = self.state / "kubeconfig"
        fallback = ROOT / ".local/bin/kind"
        self.kind = os.environ.get("KIND_BIN") or shutil.which("kind") or str(fallback)
        self.kubectl = ["kubectl", "--kubeconfig", str(self.kubeconfig), "--context", f"kind-{name}"]

    def run(self, *args, **kwargs):
        return run(*args, **kwargs)

    def ensure_cluster(self):
        for tool in ["docker", "kubectl", self.kind]:
            if not shutil.which(tool):
                raise ValueError(f"Required tool not found: {tool}")
        clusters = run(self.kind, "get", "clusters", capture=True).splitlines()
        if self.name in clusters:
            self.require_owned()
            # Docker port mappings are fixed at node creation. Never recreate a cluster implicitly.
            bindings = json.loads(run("docker", "inspect", f"{self.name}-control-plane",
                                      "--format", "{{json .HostConfig.PortBindings}}", capture=True))
            config = yaml.safe_load((LOCAL / "kind.yaml").read_text())
            for port in config["nodes"][0]["extraPortMappings"]:
                expected = {"HostIp": port["listenAddress"], "HostPort": str(port["hostPort"])}
                if expected not in bindings.get(f'{port["containerPort"]}/tcp', []):
                    raise ValueError(
                        f"Existing cluster {self.name} has no matching local port mappings. "
                        "It was left unchanged. Use another --cluster name, or explicitly "
                        "back up and recreate the old cluster; see deploy/kubernetes/README.md."
                    )
            if any(not node["State"]["Running"] for node in self.owned_nodes()):
                self.start(wait_runtime=False)
        else:
            self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
            run(self.kind, "create", "cluster", "--name", self.name, "--config", LOCAL / "kind.yaml",
                "--kubeconfig", self.kubeconfig, "--wait", "120s")

    def require_owned(self):
        if not self.kubeconfig.is_file():
            raise ValueError(f"No checkout-owned kubeconfig for {self.name}. Run hibana platform install first.")

    def owned_nodes(self):
        self.require_owned()
        ids = run("docker", "ps", "-aq", "--filter", f"label=io.x-k8s.kind.cluster={self.name}", capture=True).split()
        if not ids:
            raise ValueError(f"Local cluster {self.name} is not installed.")
        nodes = json.loads(run("docker", "inspect", *ids, capture=True))
        config = json.loads(self.kube("config", "view", "--minify", "-o", "json", capture=True))
        endpoint = urlsplit(config["clusters"][0]["cluster"]["server"])
        control = next((n for n in nodes if n["Name"] == f"/{self.name}-control-plane"), None)
        ports = control["HostConfig"]["PortBindings"].get("6443/tcp", []) if control else []
        if endpoint.hostname not in ("127.0.0.1", "localhost") or not any(p["HostPort"] == str(endpoint.port) for p in ports):
            raise ValueError("Kubeconfig does not point to this owned kind cluster; no changes made.")
        if any(not n["Name"].startswith(f"/{self.name}-") for n in nodes):
            raise ValueError("Unexpected kind node names; no changes made.")
        return nodes

    def start(self, wait_runtime=True):
        nodes = self.owned_nodes()
        stopped = [n["Id"] for n in nodes if not n["State"]["Running"]]
        resumed_at = datetime.now(timezone.utc) if stopped else None
        if stopped:
            run("docker", "start", *stopped)
        # Probe the API before kubectl wait; persisted Ready conditions alone are insufficient.
        deadline = time.monotonic() + 120
        while True:
            try:
                self.kube("get", "--raw", "/readyz", "--request-timeout=5s", capture=True, quiet=True)
                break
            except subprocess.CalledProcessError:
                if time.monotonic() >= deadline:
                    raise ValueError("Kubernetes did not resume within 120 seconds.") from None
                time.sleep(2)
        self.kube("wait", "--for=condition=Ready", "node", "--all", "--timeout=120s")
        if not wait_runtime:
            # Installation will repair absent/broken workloads after node resume.
            return
        self.wait_workloads(resumed_at, {n["Name"].lstrip("/") for n in nodes if not n["State"]["Running"]})
        for component in ("control-plane", "worker"):
            self.kube("-n", "hibana", "rollout", "status", f"deployment/hibana-{component}", "--timeout=180s")
        self.resume_admission()
        print(f"Started local cluster {self.name}. Data was preserved.")

    def pause_admission(self):
        maintenance = Maintenance(self)
        try:
            maintenance.control_plane()
        except NoControlPlane:
            print("No Control Plane is running. The owned local cluster will stop using container termination grace periods.")
            return
        path = self.state / "maintenance.json"
        if path.exists():
            owner = json.loads(path.read_text())["owner"]
        else:
            owner = secrets.token_hex(16)
            with path.open("x") as stream:
                os.chmod(path, 0o600)
                json.dump({"owner": owner}, stream)
        maintenance.close(owner)
        maintenance.drain()

    def resume_admission(self):
        path = self.state / "maintenance.json"
        if not path.exists():
            return
        owner = json.loads(path.read_text())["owner"]
        maintenance = Maintenance(self)
        maintenance.close(owner)
        maintenance.prepare(owner)
        maintenance.open(owner)
        path.unlink()

    def wait_workloads(self, resumed_at=None, restarted_nodes=None):
        """Do not trust persisted Pod/Deployment Ready conditions after a node restart."""
        deployments = json.loads(self.kube("-n", "hibana", "get", "deployments", "-o", "json", capture=True))["items"]
        expected = {d["metadata"]["name"]: d["spec"].get("replicas", 1) for d in deployments
                    if d["metadata"]["name"] in {"hibana-control-plane", "hibana-worker", "hibana-postgres", "hibana-redis", "hibana-minio"}}
        deadline = time.monotonic() + 300
        while True:
            pods = json.loads(self.kube("-n", "hibana", "get", "pods", "-o", "json", capture=True))["items"]
            if workloads_ready(pods, expected, resumed_at, restarted_nodes):
                return
            if time.monotonic() >= deadline:
                raise ValueError("Hibana workloads did not become healthy after restart within 300 seconds")
            time.sleep(2)

    def stop(self):
        nodes = self.owned_nodes()
        running = [n["Id"] for n in nodes if n["State"]["Running"]]
        if running:
            self.pause_admission()
            run("docker", "stop", "--timeout", "60", *running)
        if any(n["State"]["Running"] for n in self.owned_nodes()):
            raise ValueError("Some kind nodes are still running.")
        print(f"Stopped local cluster {self.name}. Data is preserved; use hibana platform start --source {shlex.quote(str(ROOT))} --cluster {self.name} to resume.")

    def uninstall(self):
        nodes = self.owned_nodes()
        if nodes and all(n["State"]["Running"] for n in nodes):
            self.pause_admission()
        run(self.kind, "delete", "cluster", "--name", self.name, "--kubeconfig", self.kubeconfig)
        remaining = run("docker", "ps", "-aq", "--filter", f"label=io.x-k8s.kind.cluster={self.name}", capture=True).strip()
        if remaining:
            raise ValueError("Cluster deletion is incomplete; local state was retained.")
        for name in ("kubeconfig", "sdk.env", "secrets.json", "maintenance.json"):
            (self.state / name).unlink(missing_ok=True)
        print(f"Uninstalled local cluster {self.name}.")

    def credentials(self):
        self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.state.chmod(0o700)
        target = self.state / "secrets.json"
        sdk = self.state / "sdk.env"
        if target.exists() or sdk.exists():
            if not target.is_file() or not sdk.is_file():
                raise ValueError(f"Incomplete credentials in {self.state}; restore the missing file. Keys were not rotated.")
            target.chmod(0o600)
            sdk.chmod(0o600)
            return json.loads(target.read_text())

        admin, app, redis, s3, bootstrap, login = [secrets.token_hex(24) for _ in range(6)]
        data = {
            "hibana-runtime": {"DATABASE_URL": f"postgres://faas_app:{app}@hibana-postgres:5432/hibana"},
            "hibana-control-plane": {
                "REDIS_URL": f"redis://:{redis}@hibana-redis:6379", "S3_ACCESS_KEY": "hibana-local",
                "S3_SECRET_KEY": s3, "BOOTSTRAP_ADMIN_TOKEN": bootstrap,
                "JOB_SIGNING_KEY": secrets.token_hex(32), "SECRETS_MASTER_KEY": secrets.token_hex(32),
            },
            "hibana-migration": {"MIGRATION_DATABASE_URL": f"postgres://hibana_admin:{admin}@hibana-postgres:5432/hibana"},
            "hibana-local-dependencies": {
                "POSTGRES_PASSWORD": admin, "POSTGRES_APP_PASSWORD": app, "REDIS_PASSWORD": redis,
                "MINIO_ROOT_USER": "hibana-local", "MINIO_ROOT_PASSWORD": s3,
            },
        }
        result = {"apiVersion": "v1", "kind": "List", "items": [
            {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name, "namespace": "hibana"},
             "type": "Opaque", "stringData": values} for name, values in data.items()
        ]}
        write_private(target, json.dumps(result))
        write_private(sdk, "".join(f"{key}={shlex.quote(value)}\n" for key, value in {
            "HIBANA_URL": "http://127.0.0.1:18080", "HIBANA_TENANT": "smoke",
            "HIBANA_EMAIL": "admin@example.com", "HIBANA_PASSWORD": login,
            "HIBANA_INGRESS_DOMAIN": "hibana.local", "BOOTSTRAP_ADMIN_TOKEN": bootstrap,
        }.items()))
        return result

    def sdk_env(self):
        path = self.state / "sdk.env"
        if not path.is_file():
            raise ValueError("Development credentials are missing. Run hibana platform install first.")
        env = os.environ.copy()
        # Read the generated shell-compatible file without executing it.
        for entry in shlex.split(path.read_text(), comments=True):
            key, value = entry.split("=", 1)
            env[key] = value
        env.pop("HIBANA_TOKEN", None)
        env["GATEWAY"] = "http://127.0.0.1:18084"
        return env

    def migrate(self, image):
        current = self.kube("-n", "hibana", "get", "job", "hibana-migrate", "--ignore-not-found", "-o", "json", capture=True)
        if current:
            if json.loads(current).get("status", {}).get("succeeded", 0) != 1:
                raise ValueError("Migration has not succeeded. Inspect job/hibana-migrate before retrying; it was not deleted.")
            self.kube("-n", "hibana", "delete", "job", "hibana-migrate")
        self.apply(self.render(LOCAL / "migration", image))
        self.kube("-n", "hibana", "wait", "--for=condition=complete", "job/hibana-migrate", "--timeout=600s")

    def bootstrap(self):
        env = self.sdk_env()

        def post(path, body, token=None):
            headers = {"Content-Type": "application/json"}
            if token:
                headers["Authorization"] = f"Bearer {token}"
            request = urllib.request.Request(env["HIBANA_URL"] + path, data=json.dumps(body).encode(), headers=headers)
            try:
                with urllib.request.urlopen(request, timeout=15) as response:
                    return response.status
            except urllib.error.HTTPError as error:
                return error.code

        login = {"tenant_slug": env["HIBANA_TENANT"], "email": env["HIBANA_EMAIL"], "password": env["HIBANA_PASSWORD"]}
        status = post("/auth/login", login)
        if status in (200, 201):
            return
        if status != 401:
            raise ValueError(f"Development tenant login failed: HTTP {status}")
        status = post("/admin/tenants", {
            "slug": env["HIBANA_TENANT"], "name": "Kubernetes Smoke Tenant",
            "admin_email": env["HIBANA_EMAIL"], "admin_password": env["HIBANA_PASSWORD"],
        }, env["BOOTSTRAP_ADMIN_TOKEN"])
        if status not in (201, 409) or post("/auth/login", login) not in (200, 201):
            raise ValueError(f"Development tenant bootstrap failed (HTTP {status}); check existing credentials.")

    def dependency_path(self):
        current = self.kube("-n", "hibana", "get", "deployment", "hibana-postgres", "hibana-minio",
                            "--ignore-not-found", "-o", "json", capture=True)
        items = json.loads(current)["items"] if current.strip() else []
        persistent = [any("persistentVolumeClaim" in v for v in d["spec"]["template"]["spec"]["volumes"]) for d in items]
        if persistent and not all(persistent):
            if any(persistent):
                raise ValueError("Mixed storage configuration; finish the storage migration before startup.")
            print("Existing ephemeral data preserved. Use scripts/k8s_resilience.py persist for a verified PVC migration.")
            return LOCAL / "dependencies"
        return LOCAL.parent / "persistent-dependencies"

    def preflight(self):
        for tool in ["docker", "kubectl", self.kind]:
            if not shutil.which(tool):
                raise ValueError(f"Required tool not found: {tool}. Install it and ensure it is on PATH.")
        run("docker", "info", "--format", "{{.ServerVersion}}", capture=True, timeout=20)
        self.render(LOCAL, "hibana-platform:preflight")
        self.render(LOCAL / "migration", "hibana-platform:preflight")
        print("Local checks passed: Docker, kind, kubectl and Kubernetes manifests.")

    def preview(self, action):
        if action == "install":
            self.preflight()
            clusters = run(self.kind, "get", "clusters", capture=True).splitlines()
            if self.name in clusters:
                self.owned_nodes()
            print(f"  {'reuse' if self.name in clusters else 'create'} kind cluster {self.name}")
            print("  build and load the platform image; prepare credentials and persistent dependencies")
            print("  run the database migration; deploy Control Plane and Worker; verify HTTP and tenant login")
            print("Image and generated-credential diffs require the build and are not computed in this preview.")
        else:
            nodes = self.owned_nodes()
            for node in nodes:
                print(f"  {action} {node['Name']} (currently {'running' if node['State']['Running'] else 'stopped'})")
            if action == "uninstall":
                print("  delete the owned cluster's data and local credentials")
        print("Dry run complete. No resources were changed.")

    def install(self):
        self.install_phase = "local preflight"
        self.preflight()
        self.install_phase = "cluster creation"
        self.ensure_cluster()
        credentials = self.credentials()
        self.install_phase = "image build"
        print("Installing: image build")
        # A fresh provenance attestation would change the image index even for a cached build.
        run("docker", "build", "--provenance=false", "-t", "hibana-platform:dev", ".")
        digest = run("docker", "image", "inspect", "hibana-platform:dev", "--format", "{{.Id}}", capture=True).strip()
        if not re.fullmatch(r"sha256:[a-f0-9]{64}", digest):
            raise ValueError("Docker returned an unexpected image ID.")
        image = f"hibana-platform:local-{digest.split(':')[1]}"
        run("docker", "tag", "hibana-platform:dev", image)
        run(self.kind, "load", "docker-image", image, "--name", self.name)
        docs = self.render(LOCAL, image)
        stamp_runtime_settings(docs, credentials["items"])
        self.apply([doc for doc in docs if doc["kind"] == "Namespace"])
        self.apply(credentials["items"])
        self.install_phase = "dependencies"
        print("Installing: dependencies")
        self.kube("apply", "-k", self.dependency_path())
        for dependency in ["postgres", "redis", "minio"]:
            self.kube("-n", "hibana", "rollout", "status", f"deployment/hibana-{dependency}", "--timeout=180s")
        self.kube("-n", "hibana", "wait", "--for=condition=complete", "job/hibana-local-bucket", "--timeout=180s")
        self.install_phase = "database migration"
        print("Installing: database migration")
        self.migrate(image)
        self.install_phase = "workload rollout"
        print("Installing: workload rollout")
        self.apply(docs)
        for component in ["control-plane", "worker"]:
            self.kube("-n", "hibana", "rollout", "status", f"deployment/hibana-{component}", "--timeout=300s")
        self.resume_admission()
        wait_http("http://127.0.0.1:18080/readyz", 200)
        wait_http("http://127.0.0.1:18084/", 404, {"Host": "startup-probe.smoke.hibana.local"})
        self.bootstrap()
        print(f"Ready. API: http://127.0.0.1:18080 | Apps: http://127.0.0.1:18084\nCredentials: {self.state / 'sdk.env'}\nStop: hibana platform stop --source {shlex.quote(str(ROOT))} --cluster {self.name}")

    def status(self):
        nodes = self.owned_nodes()
        for node in nodes:
            print(f'{node["Name"].lstrip("/")}: {"running" if node["State"]["Running"] else "stopped"}')
        if any(not node["State"]["Running"] for node in nodes):
            return
        self.kube("-n", "hibana", "get", "pods,jobs,services,pdb", "-o", "wide")

    def test(self):
        self.require_owned()
        env = self.sdk_env()
        run("npm", "ci", "--prefix", "sdk")
        run("node", "scripts/smoke.mjs", env=env)


def workloads_ready(pods, expected, resumed_at, restarted_nodes=None):
    ready = {name: 0 for name in expected}
    for pod in pods:
        name = pod["metadata"].get("labels", {}).get("app.kubernetes.io/name")
        if name not in ready or pod["metadata"].get("deletionTimestamp"):
            continue
        statuses = pod.get("status", {}).get("containerStatuses", [])
        if not statuses or not all(c.get("ready") and "running" in c.get("state", {}) for c in statuses):
            continue
        if resumed_at and (restarted_nodes is None or pod.get("spec", {}).get("nodeName") in restarted_nodes) and any(datetime.fromisoformat(c["state"]["running"]["startedAt"].replace("Z", "+00:00")) < resumed_at for c in statuses):
            continue
        ready[name] += 1
    return bool(expected) and all(ready[name] >= replicas for name, replicas in expected.items())


def write_private(path, contents):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        stream.write(contents)


def wait_http(url, expected, headers=None):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers or {}), timeout=5) as response:
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
        except (OSError, urllib.error.URLError):
            status = None
        if status == expected:
            return
        time.sleep(1)
    raise ValueError(f"Local endpoint did not become ready: {url}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["install", "start", "stop", "uninstall", "status", "test"])
    parser.add_argument("--cluster", default="hibana", help="kind cluster name; each name has separate local state")
    parser.add_argument("--kubeconfig", type=Path)
    parser.add_argument("--context")
    parser.add_argument("--overlay", type=Path)
    parser.add_argument("--image")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    os.umask(0o077)
    try:
        if args.kubeconfig or args.context:
            from existing import ExistingCluster
            target = ExistingCluster(args.kubeconfig, args.context, args.overlay, args.image)
        else:
            target = LocalCluster(args.cluster)
        if args.dry_run:
            target.preview(args.action)
        else:
            getattr(target, args.action)()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"Error: {error}", file=sys.stderr)
        if "target" in locals():
            print(f"Stopped during: {getattr(target, 'install_phase', args.action)}", file=sys.stderr)
        print("Retry after fixing the issue: " + shlex.join(["hibana", "platform", args.action, "--source", str(ROOT), "--cluster", args.cluster] + (["--dry-run"] if args.dry_run else [])), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
