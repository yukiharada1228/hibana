#!/usr/bin/env python3
"""CLI installation regressions on a disposable kind cluster with real dependencies.

Creates its own cluster and kubeconfig; never uses a default Kubernetes context.
Reports contain no credentials. Site credentials stay in the private, ignored run directory.
"""
import argparse
import base64
from copy import deepcopy
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import time
from urllib.request import urlopen

import yaml

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / "scripts"), str(ROOT / "sdk/platform")]
from kubernetes import LocalCluster, LOCAL
from k8s_resilience import Operations

RUN_FOLDER = None


class CommandFailure(ValueError):
    def __init__(self, program, code, stderr):
        super().__init__(f"{os.path.basename(str(program))} failed (exit {code})")
        self.stderr = stderr.decode(errors="replace")


def run(args, *, timeout, input=None, stdout=subprocess.PIPE, during=None):
    child = subprocess.Popen(args, stdin=subprocess.PIPE if input is not None else None,
                             stdout=stdout, stderr=subprocess.PIPE, start_new_session=True)
    try:
        if during:
            during(child)
        output, error = child.communicate(input, timeout=timeout)
    except BaseException:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.communicate(timeout=5)
        raise
    if child.returncode:
        # Admission errors can quote submitted configuration; keep diagnostics private.
        if RUN_FOLDER:
            with (RUN_FOLDER / "command-errors.log").open("ab") as log:
                log.write(os.path.basename(str(args[0])).encode() + b"\n" + error + b"\n")
        raise CommandFailure(args[0], child.returncode, error)
    return output


def command(*args, timeout=60, input=None):
    return run(list(map(str, args)), timeout=timeout, input=input).decode().strip()


def write_yaml(path, value):
    path.write_text(yaml.safe_dump(value, sort_keys=False))


def main():
    global RUN_FOLDER
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, help="Locally available Hibana platform image")
    parser.add_argument("--node-image", help="Optional cached kind node image")
    args = parser.parse_args()
    os.umask(0o077)
    os.environ["PYTHONUNBUFFERED"] = "1"
    name = "hibana-install-check-" + secrets.token_hex(4)
    folder = ROOT / ".local/platform-install" / name
    folder.mkdir(parents=True, mode=0o700)
    RUN_FOLDER = folder
    cluster = LocalCluster(name)
    cluster.state = folder / "state"
    cluster.state.mkdir(mode=0o700)
    cluster.kubeconfig = cluster.state / "kubeconfig"
    cluster.kubectl = ["kubectl", "--kubeconfig", str(cluster.kubeconfig), "--context", "kind-" + name]
    site = folder / "site"
    report = {"cluster": name, "started_at": datetime.now(timezone.utc).isoformat(), "passed": False,
              "scope": "Disposable single-node kind; actual CLI; PostgreSQL, Redis and MinIO in a separate namespace", "checks": []}
    created = False

    def kube(*args, **kwargs):
        return command(*cluster.kubectl, *args, **kwargs)

    def get(kind, resource):
        raw = kube("-n", "hibana", "get", kind, resource, "--ignore-not-found", "-o", "json")
        return json.loads(raw) if raw else None

    def install(label, *, preview=False, failure=None, during=None):
        print(label, flush=True)
        argv = ["node", ROOT / "sdk/src/cli.mjs", "platform", "install", "--kubeconfig", cluster.kubeconfig,
                "--context", "kind-" + name, "--overlay", site, "--image", args.image]
        if preview:
            argv.append("--dry-run")
        log = folder / (label + ".log")
        failed, diagnostics = False, ""
        with log.open("wb") as output:
            try:
                run(list(map(str, argv)), stdout=output, timeout=900, during=during)
            except CommandFailure as error:
                failed = True
                diagnostics = error.stderr
        if failed != bool(failure) or (failure and failure not in diagnostics):
            raise AssertionError(f"Unexpected CLI result in {label}; inspect the private run directory")
        report["checks"].append(label)
        return log.read_text()

    def state():
        return json.loads(get("configmap", "hibana-platform")["data"]["state.json"])["install"]

    def temporary_policies():
        return [p for p in json.loads(kube("-n", "hibana", "get", "networkpolicies", "-o", "json"))["items"]
                if p["metadata"]["name"].startswith("hibana-install-network-")]

    def eventually(check, label, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if check():
                return
            time.sleep(0.5)
        raise AssertionError("Timed out: " + label)

    def probe(address):
        script = "const s=require('net').connect(5432,process.argv[1]);s.setTimeout(1500);s.once('connect',()=>{console.log('open');s.destroy()});for(const e of ['error','timeout'])s.once(e,()=>{console.log('blocked');s.destroy()});"
        return kube("-n", "hibana", "exec", "hibana-network-probe", "--", "node", "-e", script, address) == "open"

    def lifecycle(action, label, failure=None):
        print(label, flush=True)
        argv = ["node", ROOT / "sdk/src/cli.mjs", "platform", action,
                "--kubeconfig", cluster.kubeconfig, "--context", "kind-" + name]
        if action == "uninstall":
            argv.append("--yes")
        failed, diagnostics = False, ""
        with (folder / (label + ".log")).open("wb") as output:
            try:
                run(list(map(str, argv)), stdout=output, timeout=900)
            except CommandFailure as error:
                failed, diagnostics = True, error.stderr
        if failed != bool(failure) or (failure and failure not in diagnostics):
            raise AssertionError(f"Unexpected CLI result in {label}; inspect the private run directory")
        report["checks"].append(label)

    try:
        if name in command(cluster.kind, "get", "clusters").splitlines():
            raise ValueError("Test cluster name already exists; no changes made")
        report["image_id"] = command("docker", "image", "inspect", args.image, "--format", "{{.Id}}")
        config = folder / "kind.yaml"
        write_yaml(config, {"kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4", "nodes": [{"role": "control-plane"}]})
        print("Creating isolated cluster " + name, flush=True)
        created = True
        node_image = ["--image", args.node_image] if args.node_image else []
        command(cluster.kind, "create", "cluster", "--name", name, "--config", config,
                "--kubeconfig", cluster.kubeconfig, "--wait", "120s", *node_image, timeout=240)
        report["kubernetes"] = json.loads(kube("get", "--raw", "/version"))["gitVersion"]
        images = [args.image, "postgres:16", "redis:7-alpine", "minio/minio:latest", "minio/mc:latest", "node:24-bookworm-slim"]
        print("Loading cached platform and dependency images", flush=True)
        architecture = json.loads(kube("get", "nodes", "-o", "json"))["items"][0]["status"]["nodeInfo"]["architecture"]
        # Docker Desktop may cache only one architecture of a multi-platform index.
        archive = folder / "images.tar"
        command("docker", "image", "save", "--platform", "linux/" + architecture, "-o", archive, *images, timeout=180)
        command(cluster.kind, "load", "image-archive", archive, "--name", name, timeout=300)
        archive.unlink()

        # Dependencies precede the platform and live outside the Hibana namespace.
        dependency_namespace = "hibana-install-dependencies"
        kube("create", "namespace", dependency_namespace)
        credentials = cluster.credentials()["items"]
        dependency_secret = deepcopy(next(d for d in credentials if d["metadata"]["name"] == "hibana-local-dependencies"))
        dependency_secret["metadata"]["namespace"] = dependency_namespace
        dependencies = list(yaml.safe_load_all(command("kubectl", "kustomize", LOCAL / "dependencies")))
        for doc in dependencies:
            doc["metadata"]["namespace"] = dependency_namespace
            if doc["kind"] == "NetworkPolicy":
                for ingress in doc["spec"].get("ingress", []):
                    ingress.setdefault("from", []).append({
                        "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "hibana"}},
                        "podSelector": {"matchLabels": {"app.kubernetes.io/part-of": "hibana"}}})
            for container in doc.get("spec", {}).get("template", {}).get("spec", {}).get("containers", []):
                if container["image"].startswith("minio/"):
                    container["image"] = container["image"].split("@")[0] + ":latest"
                container["imagePullPolicy"] = "IfNotPresent"
        kube("apply", "-f", "-", input=json.dumps({"apiVersion": "v1", "kind": "List", "items": [dependency_secret, *dependencies]}).encode())
        for dependency in ("postgres", "redis", "minio"):
            kube("-n", dependency_namespace, "rollout", "status", "deployment/hibana-" + dependency, "--timeout=180s", timeout=200)
        kube("-n", dependency_namespace, "wait", "--for=condition=complete", "job/hibana-local-bucket", "--timeout=180s", timeout=200)

        # Two independent IP endpoints for the same database keep this a network
        # cutover test, without pretending to test replication or data migration.
        proxy_ips = []
        for endpoint in ("old", "new"):
            pod_name = "hibana-db-" + endpoint
            proxy = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": pod_name,
                "namespace": dependency_namespace, "labels": {"app.kubernetes.io/part-of": "hibana"}}, "spec": {
                "automountServiceAccountToken": False, "containers": [{"name": "proxy", "image": "node:24-bookworm-slim",
                "command": ["node", "-e", "const net=require('net');net.createServer(c=>{const s=net.connect(5432,'hibana-postgres');c.pipe(s).pipe(c);c.on('error',()=>s.destroy());s.on('error',()=>c.destroy());c.on('close',()=>s.destroy());s.on('close',()=>c.destroy())}).listen(5432,'0.0.0.0')"],
                "readinessProbe": {"tcpSocket": {"port": 5432}, "periodSeconds": 1},
                "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"memory": "128Mi"}}}]}}
            kube("apply", "-f", "-", input=json.dumps(proxy).encode())
            kube("-n", dependency_namespace, "wait", "--for=condition=ready", "pod/" + pod_name, "--timeout=60s", timeout=70)
            proxy_ips.append(json.loads(kube("-n", dependency_namespace, "get", "pod", pod_name, "-o", "json"))["status"]["podIP"])
        old_ip, new_ip = proxy_ips

        command("node", ROOT / "sdk/scripts/pack.mjs")
        command("node", ROOT / "sdk/src/cli.mjs", "platform", "init", site)
        for filename, secret_name in (("runtime.env", "hibana-runtime"), ("control-plane.env", "hibana-control-plane"), ("migration.env", "hibana-migration")):
            values = next(d["stringData"] for d in credentials if d["metadata"]["name"] == secret_name)
            content = "".join(f"{key}={value}\n" for key, value in values.items())
            for dependency in ("postgres", "redis"):
                content = content.replace("@hibana-" + dependency + ":", "@hibana-" + dependency + "." + dependency_namespace + ".svc:")
            content = content.replace("@hibana-postgres." + dependency_namespace + ".svc:", "@" + old_ip + ":")
            (site / filename).write_text(content)
        configmap = yaml.safe_load((site / "site.yaml").read_text())
        configmap["data"].update(S3_ENDPOINT=f"http://hibana-minio.{dependency_namespace}.svc:9000", S3_BUCKET="hibana-components", INGRESS_BASE_DOMAIN="hibana.local")
        write_yaml(site / "site.yaml", configmap)
        kustomization = yaml.safe_load((site / "kustomization.yaml").read_text())
        kustomization["resources"].remove("ingress.yaml")  # Exercise dependency verification without an Ingress.
        kustomization["resources"].append("regressions.yaml")
        kustomization["patches"].append({"path": "runtime-check.yaml"})
        write_yaml(site / "kustomization.yaml", kustomization)
        egress = yaml.safe_load((site / "egress.yaml").read_text())
        egress["spec"]["egress"] = [{"to": [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": dependency_namespace}}}],
                                     "ports": [{"protocol": "TCP", "port": port} for port in (6379, 9000)]},
                                     {"to": [{"ipBlock": {"cidr": old_ip + "/32"}}], "ports": [{"protocol": "TCP", "port": 5432}]}]
        write_yaml(site / "egress.yaml", egress)

        binary = b"\x00\xff\xfe"
        secret = {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "hibana-test-keystore"},
                  "data": {"truststore.p12": base64.b64encode(binary).decode(), "text": base64.b64encode(b"ok").decode()}}
        job = {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "hibana-setup"}, "spec": {"template": {"spec": {
            "restartPolicy": "Never", "securityContext": {"runAsNonRoot": True, "runAsUser": 10001, "seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{"name": "setup", "image": args.image, "command": ["/bin/true"],
                            "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}}}]}}}}
        def save_regressions():
            (site / "regressions.yaml").write_text(yaml.safe_dump_all([secret, job], sort_keys=False))
        save_regressions()
        patches = []
        for component in ("control-plane", "worker"):
            patches.append({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "hibana-" + component}, "spec": {
                "replicas": 1, "minReadySeconds": 1, "progressDeadlineSeconds": 30,
                "template": {"spec": {"topologySpreadConstraints": [{"topologyKey": "kubernetes.io/hostname", "minDomains": 1}], "containers": [{"name": component, "env": [
                    {"name": "EXTRA_OPTIONAL", "valueFrom": {"secretKeyRef": {"name": "hibana-absent", "key": "extra", "optional": True}}},
                    {"name": "DATABASE_URL", "valueFrom": {"configMapKeyRef": {"name": "hibana-absent", "key": "db", "optional": True}}},
                    {"name": "KEYSTORE_TEXT", "valueFrom": {"secretKeyRef": {"name": "hibana-test-keystore", "key": "text"}}}],
                    "volumeMounts": [{"name": "keystore", "mountPath": "/run/keystore", "readOnly": True}]}],
                    "volumes": [{"name": "keystore", "secret": {"secretName": "hibana-test-keystore"}}]}}}})
        # Keep old Workers alive after rollout status reports success, exercising
        # the termination window in which their old DB endpoint must remain open.
        patches[-1]["spec"]["template"]["spec"]["containers"][0]["lifecycle"] = {
            "preStop": {"exec": {"command": ["/bin/sleep", "30"]}}}
        def save_patches():
            (site / "runtime-check.yaml").write_text(yaml.safe_dump_all(patches, sort_keys=False))
        save_patches()

        assert get("namespace", "hibana") is None
        preview = install("01-fresh-preview", preview=True)
        assert "Namespaced server validation" in preview and get("namespace", "hibana") is None
        # On-prem operators commonly provision the namespace and TLS first.
        namespace = {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "hibana",
                     "labels": {"app.kubernetes.io/part-of": "hibana"}}}
        kube("create", "-f", "-", input=json.dumps(namespace).encode())
        migration_path = site / "migration/job.yaml"
        original_migration = migration_path.read_text()
        delayed_migration = list(yaml.safe_load_all(original_migration))
        migration_container = next(d for d in delayed_migration if d["kind"] == "Job")["spec"]["template"]["spec"]["containers"][0]
        migration_container["command"] = ["/bin/sh", "-c", "sleep 20; exec /usr/local/bin/hibana-control-plane --migrate-only"]
        migration_path.write_text(yaml.safe_dump_all(delayed_migration, sort_keys=False))
        def observe_initial_install(child):
            def migrating():
                assert child.poll() is None, "CLI exited before initial policies were observed"
                record = get("configmap", "hibana-platform")
                return record and json.loads(record["data"]["state.json"])["install"]["phase"] == "database migration"
            eventually(migrating, "first migration in precreated namespace", timeout=180)
            assert get("networkpolicy", "default-deny") is not None
            assert get("networkpolicy", egress["metadata"]["name"]) is not None
            assert get("deployment", "hibana-control-plane") is None
            assert get("deployment", "hibana-worker") is None
            report["checks"].append("precreated-namespace-isolated-before-workloads")
            lock = get("configmap", "hibana-platform-operation")
            assert lock["data"]["action"] == "install" and lock["data"]["owner"]
            for action in ("stop", "start", "uninstall"):
                lifecycle(action, "02-concurrent-" + action, failure="Another platform operation holds the lock")
            assert get("configmap", "hibana-platform-operation")["data"]["owner"] == lock["data"]["owner"]
            assert "paused" not in json.loads(get("configmap", "hibana-platform")["data"]["state.json"])
        install("02-first-install", during=observe_initial_install)
        migration_path.write_text(original_migration)
        assert state()["status"] == "complete"
        assert get("configmap", "hibana-platform-operation")["data"] == {"owner": ""}
        lifecycle("stop", "02a-stop-healthy-runtime")
        lifecycle("start", "02b-start-healthy-runtime")
        kube("-n", "hibana", "wait", "--for=condition=complete", "job/hibana-setup", "--timeout=60s", timeout=70)
        mounted = run([*cluster.kubectl, "-n", "hibana", "exec", "deployment/hibana-worker", "--", "cat", "/run/keystore/truststore.p12"], timeout=30)
        assert mounted == binary
        assert kube("-n", "hibana", "exec", "deployment/hibana-worker", "--", "printenv", "KEYSTORE_TEXT") == "ok"
        report["checks"].append("binary-volume-and-selected-text-key")
        setup_uid = get("job", "hibana-setup")["metadata"]["uid"]
        migration_uid = get("job", "hibana-migrate")["metadata"]["uid"]
        preview = install("03-existing-preview", preview=True)
        assert "unchanged Job/hibana-setup" in preview and "recreate  Job/hibana-migrate" in preview
        assert get("job", "hibana-migrate")["metadata"]["uid"] == migration_uid
        configmap["data"]["RUST_LOG"] = "warn"
        write_yaml(site / "site.yaml", configmap)
        install("04-update")
        assert state()["status"] == "complete" and get("job", "hibana-migrate")["metadata"]["uid"] != migration_uid
        assert get("job", "hibana-setup")["metadata"]["uid"] == setup_uid

        def worker_uid():
            pods = json.loads(kube("-n", "hibana", "get", "pods", "-l", "app.kubernetes.io/name=hibana-worker", "-o", "json"))["items"]
            return next(p["metadata"]["uid"] for p in pods if not p["metadata"].get("deletionTimestamp") and
                        any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", [])))
        before_uid = worker_uid()
        secret["data"]["text"] = base64.b64encode(b"rotated").decode()
        secret["stringData"] = {"unused": "constant"}
        save_regressions()
        install("04a-direct-secret-rotation")
        assert worker_uid() != before_uid
        assert kube("-n", "hibana", "exec", "deployment/hibana-worker", "--", "printenv", "KEYSTORE_TEXT") == "rotated"
        assert get("job", "hibana-setup")["metadata"]["uid"] == setup_uid

        network_probe = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "hibana-network-probe", "namespace": "hibana",
            "labels": {"app.kubernetes.io/part-of": "hibana"}}, "spec": {"automountServiceAccountToken": False,
            "securityContext": {"runAsNonRoot": True, "runAsUser": 10001, "seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{"name": "probe", "image": "node:24-bookworm-slim", "command": ["node", "-e", "setInterval(()=>{},1000)"],
            "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}},
            "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"memory": "128Mi"}}}]}}
        kube("apply", "-f", "-", input=json.dumps(network_probe).encode())
        kube("-n", "hibana", "wait", "--for=condition=ready", "pod/hibana-network-probe", "--timeout=60s", timeout=70)
        eventually(lambda: probe(old_ip) and not probe(new_ip), "old endpoint allowed; new endpoint denied")
        report["checks"].append("network-policy-enforcement")

        def switch_database(before, after):
            for filename in ("runtime.env", "migration.env"):
                path = site / filename
                path.write_text(path.read_text().replace("@" + before + ":", "@" + after + ":"))
            egress["spec"]["egress"][-1]["to"][0]["ipBlock"]["cidr"] = after + "/32"
            write_yaml(site / "egress.yaml", egress)

        migration_path = site / "migration/job.yaml"
        original_migration = migration_path.read_text()
        delayed_migration = list(yaml.safe_load_all(original_migration))
        migration_container = next(d for d in delayed_migration if d["kind"] == "Job")["spec"]["template"]["spec"]["containers"][0]
        migration_container["command"] = ["/bin/sh", "-c", "sleep 15; exec /usr/local/bin/hibana-control-plane --migrate-only"]
        migration_path.write_text(yaml.safe_dump_all(delayed_migration, sort_keys=False))
        draining_uid = worker_uid()
        switch_database(old_ip, new_ip)
        def observe_cutover(child):
            def staging():
                assert child.poll() is None, "CLI exited before network transition was observed"
                return state()["phase"] == "database migration" and temporary_policies()
            eventually(staging, "network access before migration", timeout=180)
            eventually(lambda: probe(old_ip) and probe(new_ip), "both DB endpoints allowed during migration", timeout=10)
            live_policy = get("networkpolicy", egress["metadata"]["name"])
            assert live_policy["spec"]["egress"][-1]["to"][0]["ipBlock"]["cidr"] == old_ip + "/32"
            eventually(lambda: state()["phase"] == "old Pod termination", "old Worker termination", timeout=180)
            pods = json.loads(kube("-n", "hibana", "get", "pods", "-o", "json"))["items"]
            old_worker = next(p for p in pods if p["metadata"]["uid"] == draining_uid)
            assert old_worker["metadata"].get("deletionTimestamp")
            kube("-n", "hibana", "rollout", "status", "deployment/hibana-worker", "--timeout=5s", timeout=10)
            cp_ip = next(p["status"]["podIP"] for p in pods if
                         p["metadata"].get("labels", {}).get("app.kubernetes.io/name") == "hibana-control-plane" and
                         not p["metadata"].get("deletionTimestamp"))
            result = json.loads(kube("-n", "hibana", "exec", old_worker["metadata"]["name"], "-c", "worker", "--",
                "/usr/local/bin/hibana-control-plane", "--maintenance", "check", "worker", cp_ip, timeout=20))
            assert result["checks"]["database"] is True and temporary_policies()
            assert probe(old_ip) and probe(new_ip)
            report["checks"].append("old-worker-retains-db-access-after-rollout")
        install("04b-database-network-cutover", during=observe_cutover)
        migration_path.write_text(original_migration)
        assert temporary_policies() == []
        eventually(lambda: probe(new_ip) and not probe(old_ip), "new endpoint allowed; old endpoint denied after rollout")
        report["checks"].append("network-policy-finalization")

        # Control Plane readiness stays healthy while only Worker egress breaks.
        # Fresh connections from each Worker must still prevent install success.
        good_selector = deepcopy(egress["spec"]["podSelector"])
        egress["spec"]["podSelector"].setdefault("matchExpressions", []).append({
            "key": "app.kubernetes.io/name", "operator": "NotIn", "values": ["hibana-worker"]})
        write_yaml(site / "egress.yaml", egress)
        install("04c-final-policy-blocks-worker", failure="Stopped during: dependency verification")
        assert state()["status"] == "failed" and state()["phase"] == "dependency verification"
        assert temporary_policies() == []
        with Operations(cluster).forward("control-plane", 8080) as endpoint:
            with urlopen(endpoint + "/readyz", timeout=5) as response:
                assert response.status == 200 and json.load(response) == {"db": "ok", "store": "ok"}
        report["checks"].append("healthy-control-plane-does-not-mask-worker-egress-failure")
        egress["spec"]["podSelector"] = good_selector
        write_yaml(site / "egress.yaml", egress)
        install("04d-repair-final-policy")
        assert state()["status"] == "complete" and temporary_policies() == []

        record = get("configmap", "hibana-platform")
        job["spec"]["template"]["spec"]["containers"][0]["command"] = ["/bin/false"]
        save_regressions()
        install("05-immutable-job-preview", preview=True, failure="Kubernetes rejected Job/hibana-setup")
        install("06-immutable-job-install", failure="Kubernetes rejected Job/hibana-setup")
        assert get("configmap", "hibana-platform")["metadata"]["resourceVersion"] == record["metadata"]["resourceVersion"]
        assert get("job", "hibana-setup")["metadata"]["uid"] == setup_uid
        job["spec"]["template"]["spec"]["containers"][0]["command"] = ["/bin/true"]
        save_regressions()

        worker = patches[-1]["spec"]["template"]["spec"]["containers"][0]
        worker["command"] = ["/usr/local/bin/hibana-test-missing"]
        cp = patches[0]["spec"]["template"]["spec"]["containers"][0]
        cp["command"] = ["/usr/local/bin/hibana-test-missing"]
        save_patches()
        switch_database(new_ip, old_ip)
        install("07-rollout-failure", failure="Stopped during: workload rollout")
        assert state()["status"] == "failed" and state()["phase"] == "workload rollout"
        def mixed_control_planes():
            pods = json.loads(kube("-n", "hibana", "get", "pods", "-l", "app.kubernetes.io/name=hibana-control-plane", "-o", "json"))["items"]
            statuses = [s for p in pods for s in p.get("status", {}).get("containerStatuses", []) if s["name"] == "control-plane"]
            return any(s.get("ready") and "running" in s.get("state", {}) for s in statuses) and any("waiting" in s.get("state", {}) for s in statuses)
        assert mixed_control_planes(), "Both healthy and unavailable CP containers must be present"
        report["checks"].append("healthy-and-unavailable-cp-before-stop")
        assert temporary_policies() and probe(old_ip) and probe(new_ip)
        status = command("node", ROOT / "sdk/src/cli.mjs", "platform", "status", "--kubeconfig", cluster.kubeconfig, "--context", "kind-" + name)
        assert "last installation incomplete" in status
        report["checks"].append("failed-install-status")
        lifecycle("stop", "07a-stop-broken-runtime")
        lifecycle("start", "07b-start-broken-runtime", failure="Stopped during: start")
        paused = json.loads(get("configmap", "hibana-platform")["data"]["state.json"])["paused"]
        assert paused["owner"] and paused["drained"] is False
        worker.pop("command")
        cp.pop("command")
        save_patches()
        install("08-retry")
        assert temporary_policies() == []
        eventually(lambda: probe(old_ip) and not probe(new_ip), "network access finalized after retry")
        kube("-n", "hibana", "delete", "pod", "hibana-network-probe")
        assert state()["status"] == "complete" and get("job", "hibana-setup")["metadata"]["uid"] == setup_uid
        assert "paused" not in json.loads(get("configmap", "hibana-platform")["data"]["state.json"])
        lifecycle("uninstall", "09-remove-healthy-runtime")
        cp = patches[0]["spec"]["template"]["spec"]["containers"][0]
        cp["command"] = ["/usr/local/bin/hibana-test-missing"]
        save_patches()
        install("10-cp-never-started", failure="Stopped during: workload rollout")
        lifecycle("stop", "11-stop-unavailable-cp", failure="No running Control Plane")
        assert "paused" not in json.loads(get("configmap", "hibana-platform")["data"]["state.json"])
        lifecycle("uninstall", "12-remove-failed-installation")
        assert get("configmap", "hibana-platform") is None
        assert get("deployment", "hibana-control-plane") is None
        assert get("deployment", "hibana-worker") is None
        assert get("namespace", "hibana") is not None
        dependencies_live = json.loads(kube("-n", dependency_namespace, "get", "deployments", "-o", "json"))["items"]
        assert len(dependencies_live) == 3 and all(d["status"].get("readyReplicas", 0) == 1 for d in dependencies_live)
        cp.pop("command")
        save_patches()
        install("13-reinstall-after-failed-cleanup")
        assert state()["status"] == "complete"
        kube("-n", "hibana", "patch", "deployment", "hibana-control-plane", "--type=strategic", "-p",
             json.dumps({"spec": {"template": {"spec": {"containers": [{"name": "control-plane", "command": ["/usr/local/bin/hibana-test-missing"]}]}}}}))
        eventually(mixed_control_planes, "healthy and unavailable CP containers before uninstall")
        lifecycle("uninstall", "14-remove-repaired-runtime")
        report["checks"].append("uninstall-with-healthy-and-unavailable-cp")
        install("15-final-reinstall")
        with Operations(cluster).forward("control-plane", 8080) as endpoint:
            with urlopen(endpoint + "/readyz", timeout=5) as response:
                assert response.status == 200 and json.load(response) == {"db": "ok", "store": "ok"}
        report["checks"].append("management-api-readiness-after-retry")
        report["passed"] = True
    finally:
        if created:
            print("Removing test cluster " + name, flush=True)
            try:
                command(cluster.kind, "delete", "cluster", "--name", name, "--kubeconfig", cluster.kubeconfig, timeout=120)
                report["cluster_removed"] = True
            finally:
                (folder / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        else:
            (folder / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print("Report: " + str(folder / "report.json"), flush=True)


if __name__ == "__main__":
    main()
