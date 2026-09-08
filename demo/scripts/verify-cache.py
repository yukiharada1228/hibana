#!/usr/bin/env python3
"""Verify preparation and first requests using the film's exact Wasm artifact.

    hibana platform install --source . --cluster hibana-preparation-check
    python3 demo/scripts/verify-cache.py

Always removes the dedicated cluster. Historical cold results remain unchanged.
"""
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import time

import record as rec

CLUSTER = "hibana-preparation-check"
STATE = rec.ROOT / ".local/preparation-verification"
APP = STATE / "hello-hono"
rec.env["HIBANA_CONFIG_HOME"] = str(APP / ".hibana/cli")
rec.env.pop("HIBANA_PROFILE", None)
PLATFORM = rec.ROOT / f".local/kubernetes-{CLUSTER}"
ARTIFACT = rec.ROOT / ".local/demo-recording/hello-hono/.hibana/build/app.wasm"
rec.RECORD = rec.DEMO / "public/preparation-verification.json"
rec.data = {
    "format": 1, "kind": "preparation-verification",
    "recordedAt": datetime.now(timezone.utc).isoformat(),
    "cluster": CLUSTER, "steps": [], "requests": [], "proof": {}, "complete": False,
}
KUBE = ["kubectl", "--kubeconfig", str(PLATFORM / "kubeconfig"), "--context", f"kind-{CLUSTER}"]
FORWARDS = []
PORTS = {}
LOGS = {}


def command(id, args, **kwargs):
    if args[0] == "kubectl":
        args = [args[0], "--request-timeout=15s", *args[1:]]
    return rec.command(id, args, **kwargs)


def metric_values(output):
    metrics = {}
    for line in output.splitlines():
        if line and not line.startswith("#"):
            name, value = line.split()[:2]
            metrics[name] = float(value)
    return {
        "miss": metrics.get("wasmtime_component_cache_misses_total", 0),
        "lru": metrics.get('wasmtime_component_cache_hits_total{tier="lru"}', 0),
        "cwasm": metrics.get('wasmtime_component_cache_hits_total{tier="cwasm"}', 0),
    }


def snapshot(id, pods):
    return {pod: metric_values(command(f"metrics-{id}-{pod}", [
        "curl", "--fail", "--silent", "--show-error", "--max-time", "10",
        f"http://127.0.0.1:{PORTS[pod]}/metrics",
    ], timeout=30)) for pod in pods}


def start_forward(pod, port):
    path = STATE / f"{pod}-port-forward.log"
    log = path.open("w")
    child = subprocess.Popen(KUBE + ["-n", "hibana", "port-forward", "--address", "127.0.0.1",
        f"pod/{pod}", f"{port}:9090"], stdout=log, stderr=subprocess.STDOUT, env=rec.env)
    FORWARDS.append((child, log))
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline and child.poll() is None:
        if f"Forwarding from 127.0.0.1:{port}" in path.read_text():
            PORTS[pod] = port
            return
        time.sleep(0.1)
    raise RuntimeError(f"Port forwarding failed for {pod}: {path.read_text()}")


def worker_pods(id):
    return json.loads(command(id, KUBE + ["-n", "hibana", "get", "pods",
        "-l", "app.kubernetes.io/name=hibana-worker", "-o", "json"]))["items"]


def invoke(phase):
    index = len(rec.data["requests"]) + 1
    output = command(f"request-{index}", ["curl", "--silent", "--show-error", "--max-time", "3",
        "-i", "-w", "\n__TIME_TOTAL__%{time_total}\n", "-H",
        "Host: hello-hono.smoke.hibana.local", "http://127.0.0.1:18084/"], cwd=APP, timeout=10)
    http, timing = output.rsplit("\n__TIME_TOTAL__", 1)
    headers, body = re.split(r"\r?\n\r?\n", http, maxsplit=1)
    row = {"request": index, "phase": phase, "httpStatus": int(headers.split()[1]),
           "httpTimeMs": round(float(timing.strip()) * 1000, 3), "response": json.loads(body)}
    rec.data["requests"].append(row)
    rec.save()
    assert row["httpStatus"] == 200 and row["response"] == {"message": "Hello, Wasm!", "count": 1}, row
    assert row["httpTimeMs"] < 2000, row
    return row


def collect_logs(pods):
    for pod in pods:
        LOGS[pod] = command(f"logs-{pod}", KUBE + ["-n", "hibana", "logs", pod, "--timestamps=true"])


def verify_new_worker(phase, previous, digest):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        documents = worker_pods(f"{phase}-pods")
        added = [p for p in documents if p["metadata"]["name"] not in previous
                 and any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", []))]
        if len(added) == 1:
            break
        time.sleep(0.5)
    else:
        raise RuntimeError(f"{phase}: replacement Worker did not become ready")
    pod = added[0]["metadata"]["name"]
    start_forward(pod, 19190 + len(PORTS))
    # No warm-up HTTP request or manual /prepare. The Control Plane's reconciler
    # must create the native cache while regular traffic stays on prepared Pods.
    observations = []
    while time.monotonic() < deadline:
        row = invoke(phase + "-preparing")
        files = command(f"{phase}-cache-{row['request']}", KUBE + ["-n", "hibana", "exec", pod,
                        "--", "ls", "/var/cache/hibana"], timeout=25)
        observations.append({"afterRequest": row["request"], "nativeCachePresent": digest + ".cwasm" in files})
        if observations[-1]["nativeCachePresent"]:
            break
        time.sleep(0.25)
    else:
        raise RuntimeError(f"{phase}: background preparation did not complete")
    counters = snapshot(phase + "-prepared", [pod])[pod]
    assert counters["miss"] == 1, counters
    rec.data["proof"][phase] = {"pod": pod, "uid": added[0]["metadata"]["uid"],
                               "observations": observations, "preparedCounters": counters}
    for _ in range(12):
        invoke(phase + "-prepared")
    return pod


def main():
    if not (PLATFORM / "kubeconfig").is_file() or APP.exists():
        raise RuntimeError("Install the dedicated cluster and use a fresh .local/preparation-verification/hello-hono directory.")
    film = json.loads((rec.DEMO / "public/history/recording-before-preparation.json").read_text())
    digest = hashlib.sha256(ARTIFACT.read_bytes()).hexdigest()
    assert digest == film["proof"]["artifact"]["sha256"]
    rec.data["proof"]["wasmSha256"] = digest
    rec.data["proof"]["sameArtifactAsFilm"] = True
    rec.data["proof"]["sourceSha256"] = {
        str(path.relative_to(rec.ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in [rec.ROOT / "crates/worker/src/artifacts.rs",
                     rec.ROOT / "crates/worker/src/runtime/mod.rs",
                     rec.ROOT / "crates/worker/src/service.rs",
                     rec.ROOT / "crates/worker/src/direct_http.rs",
                     rec.ROOT / "crates/control-plane/src/dispatch.rs",
                     rec.ROOT / "crates/control-plane/src/preparation.rs",
                     rec.ROOT / "crates/control-plane/src/handlers/components.rs"]
    }
    try:
        for entry in shlex.split((PLATFORM / "sdk.env").read_text(), comments=True):
            key, value = entry.split("=", 1)
            rec.env[key] = value
            if any(word in key for word in ("TOKEN", "PASSWORD", "SECRET")):
                rec.hidden.append(value)
        rec.env.pop("HIBANA_TOKEN", None)
        rec.env["KUBECONFIG"] = str(PLATFORM / "kubeconfig")
        APP.mkdir(parents=True, mode=0o700)
        shutil.copyfile(ARTIFACT, APP / "app.wasm")
        (APP / "hibana.json").write_text(json.dumps({
            "name": "hello-hono", "component": "app.wasm",
            "limits": {"memory_mb": 256, "timeout_ms": 15000},
        }, indent=2) + "\n")
        command("login", ["hibana", "login"], cwd=APP)
        pod_docs = worker_pods("pods-before")
        assert len(pod_docs) == 2
        pods = sorted(p["metadata"]["name"] for p in pod_docs)
        rec.data["proof"]["workers"] = [{
            "name": p["metadata"]["name"], "uid": p["metadata"]["uid"],
            "node": p["spec"]["nodeName"],
            "imageId": p["status"]["containerStatuses"][0]["imageID"],
        } for p in pod_docs]
        for index, pod in enumerate(pods):
            start_forward(pod, 19190 + index)
        before = snapshot("baseline", pods)
        assert all(v == 0 for counts in before.values() for v in counts.values()), before
        rec.data["proof"]["baseline"] = before
        command("deploy", ["hibana", "deploy", "--version", "1.0.0"], cwd=APP)
        before = snapshot("after-deploy", pods)
        assert all(c == {"miss": 1, "lru": 0, "cwasm": 0} for c in before.values()), before
        rec.data["proof"]["afterDeploy"] = before
        count = command("no-preparation-executions", KUBE + ["-n", "hibana", "exec", "deployment/hibana-postgres", "--",
            "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", "SELECT count(*) FROM executions"])
        assert count.strip() == "0", count
        rec.data["proof"]["executionsAfterDeploy"] = 0
        for i in range(1, 9):
            row = invoke("after-deploy")
            after = snapshot(str(i), pods)
            delta = {pod: {key: after[pod][key] - before[pod][key] for key in before[pod]} for pod in pods}
            changes = [(pod, tier) for pod, counts in delta.items() for tier, value in counts.items() if value == 1]
            assert len(changes) == 1 and sum(v for counts in delta.values() for v in counts.values()) == 1, delta
            pod, tier = changes[0]
            assert tier in ("lru", "cwasm"), delta
            row.update({"pod": pod, "cache": tier, "metricsBefore": before,
                        "metricsAfter": after, "metricsDelta": delta})
            rec.save()
            print(f"MEASURED: request={i} pod={pod} cache={tier} http_ms={row['httpTimeMs']}", flush=True)
            before = after
        command("scale-workers", KUBE + ["-n", "hibana", "scale", "deployment/hibana-worker", "--replicas=3"])
        added = verify_new_worker("scale", pods, digest)
        pods.append(added)
        collect_logs(pods)
        removed = pods[0]
        command("replace-worker", KUBE + ["-n", "hibana", "delete", "pod", removed, "--wait=false"])
        replacement = verify_new_worker("replacement", pods, digest)
        pods.remove(removed)
        pods.append(replacement)
        collect_logs(pods)
        query = "SELECT json_agg(row_to_json(r)) FROM (SELECT e.id,e.status,e.http_request,e.wall_time_ms,e.peak_memory_bytes,v.wasm_sha256 FROM executions e JOIN components c ON c.id=e.component_id JOIN component_versions v ON v.id=e.version_id WHERE c.name='hello-hono' ORDER BY e.created_at) r"
        executions = json.loads(command("executions", KUBE + ["-n", "hibana", "exec", "deployment/hibana-postgres", "--",
            "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", query]))
        assert len(executions) == len(rec.data["requests"])
        for row, execution in zip(rec.data["requests"], executions):
            assert execution["status"] == "succeeded" and execution["wasm_sha256"] == digest
            owners = [pod for pod, log in LOGS.items() if execution["id"] in log]
            assert len(owners) == 1, (row, owners)
            if "pod" in row:
                assert owners == [row["pod"]], (row, owners)
            row["pod"] = owners[0]
            row["execution"] = execution
            row["podConfirmedByExecutionLog"] = True
        for phase, pod in [("scale", added), ("replacement", replacement)]:
            assert any(row["pod"] == pod and row["phase"] == phase + "-prepared" for row in rec.data["requests"])
        after = snapshot("final", pods)
        assert all(c["miss"] == 1 for c in after.values()), after
        rec.data["proof"]["finalCacheCounters"] = after
        rec.data["proof"]["finalWorkers"] = worker_pods("pods-after")
        command("delete", ["hibana", "delete", "hello-hono", "--yes"], cwd=APP)
        assert json.loads(command("list-empty", ["hibana", "list"], cwd=APP)) == []
        rec.data["proof"]["remainingApplications"] = []
        rec.data["complete"] = True
    finally:
        command("uninstall", ["hibana", "platform", "uninstall", "--source", str(rec.ROOT), "--cluster", CLUSTER, "--yes"])
        for child, log in FORWARDS:
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.terminate()
                child.wait(timeout=5)
            log.close()
        nodes = command("cluster-empty", ["docker", "ps", "-a", "--filter", f"label=io.x-k8s.kind.cluster={CLUSTER}", "--format", "{{.Names}}"])
        assert not nodes.strip()
        rec.data["proof"]["remainingNodes"] = []
        if APP.is_dir():
            shutil.rmtree(APP)
        rec.save()
    print("PASS: initial requests, Worker addition and replacement use prepared code; dedicated cluster removed", flush=True)


if __name__ == "__main__":
    main()
