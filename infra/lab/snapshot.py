#!/usr/bin/env python3
"""Record disposable cluster identities and credential hashes, never credential values."""

import hashlib
import json
import subprocess
import sys
from pathlib import Path

k = [
    "limactl",
    "shell",
    "hibana-iac-cp",
    "sudo",
    "kubectl",
    "--kubeconfig=/etc/kubernetes/admin.conf",
]


def get(args):
    return json.loads(subprocess.check_output(k + args, text=True))


report = {}
for kind in ["nodes", "pv", "pvc", "pods", "secrets"]:
    items = get(["get", kind, "-A", "-o", "json"])["items"]
    group = {}
    for d in items:
        ns = d["metadata"].get("namespace", "")
        if ns and ns not in ["hibana", "hibana-identity", "hibana-edge"]:
            continue
        if kind == "pods" and any(
            o["kind"] == "Job" for o in d["metadata"].get("ownerReferences", [])
        ):
            continue
        name = ns + "/" + d["metadata"]["name"]
        entry = {"uid": d["metadata"]["uid"]}
        if kind == "secrets":
            entry["data_sha256"] = hashlib.sha256(
                json.dumps(d.get("data", {}), sort_keys=True).encode()
            ).hexdigest()
        if kind == "pods":
            entry["phase"] = d["status"]["phase"]
            entry["restarts"] = {
                c["name"]: c["restartCount"]
                for c in d["status"].get("containerStatuses", [])
            }
        if kind == "nodes":
            entry["allocatable"] = d["status"]["allocatable"]
        group[name] = entry
    report[kind] = group
path = Path(sys.argv[1])
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(json.dumps(report, indent=2) + "\n")
print(f"Snapshot saved: {path} (no credential values)")
