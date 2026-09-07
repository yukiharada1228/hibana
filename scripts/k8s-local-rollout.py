#!/usr/bin/env python3
"""Verify HTTP requests during a Worker rolling restart in the owned kind cluster.

Run k8s-local-smoke.sh and keep k8s-local.sh forward running first.
This checks Worker Pod replacement, not physical HA or Control Plane replacement.
"""
import json
import os
import subprocess
import time
import urllib.request
from pathlib import Path

root = Path(__file__).resolve().parents[1]
kube = ["kubectl", "--kubeconfig", str(root / ".local/kubernetes/kubeconfig"),
        "--context", "kind-hibana-dev", "-n", "hibana"]


def workers():
    data = json.loads(subprocess.check_output(kube + [
        "get", "pods", "-l", "app.kubernetes.io/name=hibana-worker", "-o", "json"], text=True))
    return {pod["metadata"]["uid"] for pod in data["items"]}


def get(path):
    request = urllib.request.Request(os.environ.get("GATEWAY", "http://127.0.0.1:18084") + path,
                                     headers={"Host": "hello-hono.smoke.hibana.local"})
    with urllib.request.urlopen(request, timeout=50) as response:
        return response.read().decode()


before = workers()
assert json.loads(get("/")) == {"message": "Hello Hibana"}
subprocess.run(kube + ["rollout", "restart", "deployment/hibana-worker"], check=True)
rollout = subprocess.Popen(kube + ["rollout", "status", "deployment/hibana-worker", "--timeout=180s"])
count = 0
try:
    deadline = time.monotonic() + 180
    # Deployment availability can precede completion of old Pods' graceful shutdown.
    while count < 30 or rollout.poll() is None or not before.isdisjoint(workers()):
        assert time.monotonic() < deadline, "Worker rollout exceeded test deadline"
        assert json.loads(get("/")) == {"message": "Hello Hibana"}
        count += 1
        time.sleep(1)
    assert rollout.wait() == 0, "Worker rollout failed"
    after = workers()
    assert before.isdisjoint(after), "old Worker Pods still present"
    pods = json.loads(subprocess.check_output(kube + ["get", "pods", "-l", "app.kubernetes.io/name=hibana-worker", "-o", "json"], text=True))["items"]
    assert len({pod["spec"]["nodeName"] for pod in pods}) >= 2, "Workers must remain spread after rollout"
    assert json.loads(get("/secret")) == {"configured": True}
    print(f"PASS: {count} HTTP requests across Worker replacement; environment and Secrets passed", flush=True)
finally:
    if rollout.poll() is None:
        rollout.terminate()
        rollout.wait()
