#!/usr/bin/env python3
"""Measure the same Wasm and resource allocation before/after HTTP-path changes.

hibana platform install --source . --cluster hibana-latency-check
python3 demo/scripts/verify-latency.py setup
python3 demo/scripts/verify-latency.py measure baseline
# Install the changed build into the same dedicated cluster, then measure again.
python3 demo/scripts/verify-latency.py measure improved
# Recheck both images without another build or a resource change.
python3 demo/scripts/verify-latency.py switch baseline
python3 demo/scripts/verify-latency.py measure baseline-recheck
python3 demo/scripts/verify-latency.py switch improved
python3 demo/scripts/verify-latency.py measure improved-recheck
python3 demo/scripts/verify-latency.py cleanup
"""
from datetime import datetime, timezone
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import time

import record as rec

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--record", default="latency-verification.json", help="JSON filename under demo/public; use a new name for a new comparison")
parser.add_argument("action", choices=["setup", "measure", "switch", "restart", "cleanup"])
parser.add_argument("label", nargs="?")
args = parser.parse_args()
if Path(args.record).name != args.record or not args.record.endswith(".json"):
    parser.error("--record must be a JSON filename, not a path")
if (args.action in ("measure", "switch")) != bool(args.label):
    parser.error("measure and switch require a label; other actions do not accept one")

CLUSTER = "hibana-latency-check"
STATE = rec.ROOT / ".local/latency-verification"
APP = STATE / "hello-hono"
rec.env["HIBANA_CONFIG_HOME"] = str(APP / ".hibana/cli")
rec.env.pop("HIBANA_PROFILE", None)
PLATFORM = rec.ROOT / f".local/kubernetes-{CLUSTER}"
KUBE = ["kubectl", "--kubeconfig", str(PLATFORM / "kubeconfig"), "--context", f"kind-{CLUSTER}", "-n", "hibana"]
rec.RECORD = rec.DEMO / "public" / args.record
rec.data = json.loads(rec.RECORD.read_text()) if rec.RECORD.exists() else {
    "format": 1, "kind": "latency-verification", "recordedAt": datetime.now(timezone.utc).isoformat(),
    "cluster": CLUSTER, "steps": [], "phases": [], "proof": {}, "complete": False,
}


def command(id, args, **kwargs):
    return rec.command(id, args, **kwargs)


def credentials():
    for entry in shlex.split((PLATFORM / "sdk.env").read_text(), comments=True):
        key, value = entry.split("=", 1)
        rec.env[key] = value
        if any(word in key for word in ("TOKEN", "PASSWORD", "SECRET")):
            rec.hidden.append(value)
    rec.env.pop("HIBANA_TOKEN", None)


def pods(label):
    return json.loads(command(label, KUBE + ["get", "pods", "-o", "json"]))["items"]


def stats(label, documents):
    return {p["metadata"]["name"]: command(f"{label}-{p['metadata']['name']}", KUBE + ["exec", p["metadata"]["name"], "--", "cat",
        "/sys/fs/cgroup/cpu.stat", "/sys/fs/cgroup/cpu.max", "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.events", "/sys/fs/cgroup/cpu.pressure",
        "/sys/fs/cgroup/memory.pressure"], timeout=25) for p in documents}


def setup():
    if APP.exists() or rec.data["phases"]:
        raise RuntimeError("Inspect the existing dedicated measurement before overwriting it")
    credentials()
    APP.mkdir(parents=True, mode=0o700)
    artifact = rec.ROOT / ".local/demo-recording-current/app.wasm"
    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
    assert digest == json.loads((rec.DEMO / "public/recording.json").read_text())["proof"]["artifact"]["sha256"]
    rec.data["proof"]["wasmSha256"] = digest
    shutil.copyfile(artifact, APP / "app.wasm")
    (APP / "hibana.json").write_text(json.dumps({"name": "hello-hono", "component": "app.wasm",
        "limits": {"memory_mb": 256, "timeout_ms": 15000}}, indent=2) + "\n")
    command("enable-timings", KUBE + ["set", "env", "deployment/hibana-worker", "deployment/hibana-control-plane", "RUST_LOG=info,hibana_latency=debug"])
    for name in ["hibana-worker", "hibana-control-plane"]:
        command("rollout-" + name, KUBE + ["rollout", "status", "deployment/" + name, "--timeout=120s"])
    command("login", ["hibana", "login"], cwd=APP)
    command("deploy", ["hibana", "deploy", "--version", "1.0.0"], cwd=APP)
    rec.data["proof"]["dockerResources"] = json.loads(command("docker-resources", ["docker", "info", "--format", '{"cpus":{{.NCPU}},"memoryBytes":{{.MemTotal}}}']))
    command("vm-pressure", ["docker", "exec", CLUSTER + "-control-plane", "cat", "/proc/meminfo", "/proc/pressure/cpu", "/proc/pressure/memory"])
    rec.save()


def measure(label):
    credentials()
    if any(p["label"] == label for p in rec.data["phases"]):
        raise RuntimeError("Measurement label already exists")
    # Switching to the already active version prepares every discovered Worker
    # without invoking the guest or creating a different version/configuration.
    command(label + "-prepare", ["hibana", "rollback", "--version", "1.0.0"], cwd=APP)
    documents = [p for p in pods(label + "-pods") if p["metadata"].get("labels", {}).get("app.kubernetes.io/name")
        in ("hibana-worker", "hibana-control-plane", "hibana-postgres")]
    assert len([p for p in documents if p["metadata"]["labels"]["app.kubernetes.io/name"] == "hibana-worker"]) == 2
    phase = {"label": label, "pods": documents, "requests": [], "timings": [], "preparations": [], "complete": False}
    rec.data["phases"].append(phase)
    phase["executionsBefore"] = int(command(label + "-executions-before", KUBE + ["exec", "deployment/hibana-postgres", "--",
        "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", "SELECT count(*) FROM executions"]))
    phase["vmBefore"] = command(label + "-vm-before", ["docker", "exec", CLUSTER + "-control-plane", "cat",
        "/proc/meminfo", "/proc/pressure/cpu", "/proc/pressure/memory"])
    phase["before"] = stats(label + "-before", documents)
    for index in range(40):
        output = command(f"{label}-http-{index+1}", ["curl", "--silent", "--show-error", "--max-time", "10", "-w",
            '\n{"status":%{http_code},"total":%{time_total},"ttfb":%{time_starttransfer},"connect":%{time_connect}}\n',
            "-H", "Host: hello-hono.smoke.hibana.local", "http://127.0.0.1:18084/"], timeout=15)
        body, timing = output.strip().split("\n", 1)
        row = json.loads(timing)
        row["body"] = json.loads(body)
        phase["requests"].append(row)
        rec.save()
        assert row["status"] == 200 and row["body"] == {"message": "Hello, Wasm!", "count": 1}, row
        time.sleep(0.05)
    phase["after"] = stats(label + "-after", documents)
    phase["vmAfter"] = command(label + "-vm-after", ["docker", "exec", CLUSTER + "-control-plane", "cat",
        "/proc/meminfo", "/proc/pressure/cpu", "/proc/pressure/memory"])
    for pod in documents:
        if pod["metadata"]["labels"]["app.kubernetes.io/name"] == "hibana-postgres":
            continue
        name = pod["metadata"]["name"]
        output = command(label + "-logs-" + name, KUBE + ["logs", name])
        for line in output.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("target") == "hibana_latency":
                if event.get("stage") == "component_preparation":
                    phase["preparations"].append({"pod": name, **event})
                else:
                    phase["timings"].append({"pod": name, **event})
    query = "SELECT COALESCE(json_agg(row_to_json(r)),'[]'::json) FROM (SELECT e.id,e.status,e.wall_time_ms,v.wasm_sha256 FROM executions e JOIN component_versions v ON v.id=e.version_id ORDER BY e.created_at DESC LIMIT 40) r"
    phase["executions"] = list(reversed(json.loads(command(label + "-executions", KUBE + ["exec", "deployment/hibana-postgres", "--",
        "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", query]))))
    assert len(phase["executions"]) == 40
    assert all(e["status"] == "succeeded" and e["wasm_sha256"] == rec.data["proof"]["wasmSha256"] for e in phase["executions"])
    ids = {e["id"] for e in phase["executions"]}
    phase["timings"] = [e for e in phase["timings"] if e.get("execution_id", e.get("span", {}).get("execution_id")) in ids]
    required_stages = {"cp_admission", "cp_forward_setup", "worker_discovery", "worker_admission", "wasm_runtime", "worker_execution"}
    for execution_id in ids:
        stages = {e.get("stage") for e in phase["timings"]
            if e.get("execution_id", e.get("span", {}).get("execution_id")) == execution_id}
        assert required_stages <= stages, (execution_id, required_stages - stages)
    phase["complete"] = True
    rec.save()
    print(f"PASS {label}: 40 requests, {len(phase['timings'])} phase timings", flush=True)


def switch(label):
    phase = next(p for p in rec.data["phases"] if p["label"] == label and p["complete"])
    for deployment, container in [("hibana-control-plane", "control-plane"), ("hibana-worker", "worker")]:
        image = next(p["spec"]["containers"][0]["image"] for p in phase["pods"]
            if p["metadata"]["labels"]["app.kubernetes.io/name"] == deployment)
        assert image.startswith("hibana-platform:local-")
        command("switch-" + label + "-" + deployment, KUBE + ["set", "image", "deployment/" + deployment, container + "=" + image])
    for name in ["hibana-worker", "hibana-control-plane"]:
        command("switch-rollout-" + name, KUBE + ["rollout", "status", "deployment/" + name, "--timeout=120s"])


def restart():
    command("restart", KUBE + ["rollout", "restart", "deployment/hibana-control-plane", "deployment/hibana-worker"])
    for name in ["hibana-worker", "hibana-control-plane"]:
        command("restart-rollout-" + name, KUBE + ["rollout", "status", "deployment/" + name, "--timeout=120s"])


def cleanup():
    credentials()
    command("delete", ["hibana", "delete", "hello-hono", "--yes"], cwd=APP)
    assert json.loads(command("empty-apps", ["hibana", "list"], cwd=APP)) == []
    command("uninstall", ["hibana", "platform", "uninstall", "--source", str(rec.ROOT), "--cluster", CLUSTER, "--yes"])
    assert not command("empty-cluster", ["docker", "ps", "-a", "--filter", f"label=io.x-k8s.kind.cluster={CLUSTER}", "--format", "{{.Names}}"]).strip()
    shutil.rmtree(APP)
    rec.data["proof"]["remainingNodes"] = []
    rec.data["complete"] = all(p["complete"] for p in rec.data["phases"])
    rec.save()


if __name__ == "__main__":
    if args.action == "setup":
        setup()
    elif args.action == "measure":
        measure(args.label)
    elif args.action == "switch":
        switch(args.label)
    elif args.action == "restart":
        restart()
    elif args.action == "cleanup":
        cleanup()
