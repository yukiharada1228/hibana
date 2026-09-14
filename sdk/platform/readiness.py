"""Verify Pod termination and fresh dependency connections during installation."""
import json
import subprocess
import time

from maintenance import Maintenance


class Readiness:
    def __init__(self, target):
        self.target = target

    def terminating(self):
        items = json.loads(self.target.kube("-n", "hibana", "get", "pods", "-o", "json",
                                           capture=True, quiet=True, timeout=20))["items"]
        # Namespace-wide policies can also cover auxiliary Deployments. Completed
        # Jobs and healthy unchanged Pods do not need to disappear.
        return [p for p in items if p["metadata"].get("deletionTimestamp")
                and p.get("status", {}).get("phase") not in ("Succeeded", "Failed")]

    @staticmethod
    def identity(pods):
        return {uid: (p.get("status", {}).get("podIP"), p["metadata"].get("deletionTimestamp"),
                      p.get("status", {}).get("containerStatuses", [])) for uid, p in pods.items()}

    def wait_termination(self, timeout=None):
        pending = self.terminating()
        grace = max((p.get("spec", {}).get("terminationGracePeriodSeconds", 30) for p in pending), default=0)
        deadline = time.monotonic() + (timeout if timeout is not None else max(180, grace + 30))
        while pending:
            if time.monotonic() >= deadline:
                names = ", ".join(sorted(p["metadata"]["name"] for p in pending))
                raise ValueError(f"Old Pods are still terminating: {names}. Network access was retained; inspect the Pods and retry install.")
            time.sleep(0.5)
            pending = self.terminating()

    def verify_dependencies(self, timeout=90, stable_seconds=5):
        maintenance = Maintenance(self.target)
        deadline, healthy_since, identity = time.monotonic() + timeout, None, None
        failures = ["runtime Pods are not ready"]
        while time.monotonic() < deadline:
            fleet = {c: maintenance.pods(c) for c in ("control-plane", "worker")}
            before = {c: self.identity(pods) for c, pods in fleet.items()}
            failures = []
            for component, pods in fleet.items():
                if not pods or any(p["metadata"].get("deletionTimestamp") or not p.get("status", {}).get("podIP") or
                        not any(s.get("name") == component and s.get("ready") and "running" in s.get("state", {})
                                for s in p.get("status", {}).get("containerStatuses", [])) for p in pods.values()):
                    failures.append(f"{component} Pods are not ready")
            if not failures:
                for component, pods in fleet.items():
                    peer = "worker" if component == "control-plane" else "control-plane"
                    addresses = ",".join(sorted(p["status"]["podIP"] for p in fleet[peer].values()))
                    expected = {"database", "object_store", "workers", "redis"} if component == "control-plane" else {
                        "database", "object_store", "control_planes"}
                    for pod in pods.values():
                        name = pod["metadata"]["name"]
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            failures.append("dependency check deadline reached")
                            break
                        try:
                            raw = self.target.kube("-n", "hibana", "exec", f"pod/{name}", "-c", component, "--",
                                "/usr/local/bin/hibana-control-plane", "--maintenance", "check", component, addresses,
                                capture=True, quiet=True, timeout=min(20, remaining))
                            result = json.loads(raw)
                            if not isinstance(result, dict) or result.get("protocol") != 1 or not isinstance(result.get("checks"), dict):
                                raise ValueError("unsupported probe output")
                            failures.extend(f"{name}: {key}" for key in sorted(expected) if result["checks"].get(key) is not True)
                        except (ValueError, subprocess.SubprocessError):
                            failures.append(f"{name}: dependency probe failed (use a platform image supporting --maintenance check)")
                after = {c: self.identity(maintenance.pods(c)) for c in fleet}
                if before != after:
                    failures.append("runtime Pods changed during verification")
            if failures or identity != before:
                healthy_since = None
            identity = before
            if not failures:
                if healthy_since is None:
                    healthy_since = time.monotonic()
                if time.monotonic() - healthy_since >= stable_seconds:
                    print("Fresh dependency connections verified from every Control Plane and Worker Pod.")
                    return
            time.sleep(1)
        raise ValueError("Dependency verification failed after final NetworkPolicies: " + "; ".join(failures or ["fleet did not remain stable"]) +
                         ". Fix connectivity or credentials and rerun install.")
