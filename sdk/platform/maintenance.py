"""Close fleet admission and drain through CP-local APIs; no credentials leave Pods."""
import json
import subprocess
import time


class NoControlPlane(ValueError):
    """No CP container can currently serve an operator request."""


class Maintenance:
    def __init__(self, target):
        self.target = target

    def pods(self, component):
        items = json.loads(self.target.kube("-n", "hibana", "get", "pods", "-l",
            f"app.kubernetes.io/name=hibana-{component}", "-o", "json", capture=True, timeout=30))["items"]
        return {p["metadata"]["uid"]: p for p in items}

    def call(self, pod, *args, timeout=25):
        return self.target.kube("-n", "hibana", "exec", f"pod/{pod}", "-c", "control-plane", "--",
            "/usr/local/bin/hibana-control-plane", "--maintenance", *args, capture=True, timeout=timeout)

    def control_plane(self):
        pods = self.pods("control-plane")
        candidates = sorted((not c.get("ready", False), p["metadata"]["name"])
                            for p in pods.values() if not p["metadata"].get("deletionTimestamp")
                            for c in p.get("status", {}).get("containerStatuses", [])
                            if c["name"] == "control-plane" and "running" in c.get("state", {}))
        if not candidates:
            raise NoControlPlane("No running Control Plane; admission and replicas were not changed")
        return candidates[0][1]

    def close(self, owner):
        self.call(self.control_plane(), "close", owner)

    def open(self, owner):
        self.call(self.control_plane(), "open", owner)

    def drain(self, timeout=180):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            before = self.pods("control-plane")
            if not before:
                raise ValueError("Control Plane disappeared during drain; replicas were not stopped")
            busy = False
            checked = 0
            for pod in before.values():
                if not any(c.get("name") == "control-plane" and "running" in c.get("state", {})
                           for c in pod.get("status", {}).get("containerStatuses", [])):
                    # Waiting/terminated containers cannot retain a request.
                    # Running containers, including unready/terminating ones,
                    # must report their local requests and durable executions.
                    continue
                status = json.loads(self.call(pod["metadata"]["name"], "status",
                                              timeout=min(25, max(1, deadline - time.monotonic()))))
                checked += 1
                for key in ("active_requests", "inflight_executions"):
                    if type(status.get(key)) is not int or status[key] < 0:
                        raise ValueError("Invalid drain status; upgrade the platform image before stopping")
                    busy |= status[key] != 0
            if not checked:
                raise ValueError("No running Control Plane during drain; execution status is unknown and replicas were not stopped")
            if not busy and self.identity(before) == self.identity(self.pods("control-plane")):
                return
            time.sleep(0.25)
        raise ValueError("Drain timed out; admission remains closed and replicas are retained. Retry stop or start.")

    @staticmethod
    def identity(pods):
        return {uid: (p.get("status", {}).get("podIP"), p.get("status", {}).get("phase"),
            p["metadata"].get("deletionTimestamp"), tuple(
            (c["name"], c.get("restartCount", 0), c.get("containerID"), c.get("ready", False),
             json.dumps(c.get("state", {}), sort_keys=True))
            for c in p.get("status", {}).get("containerStatuses", []))) for uid, p in pods.items()}

    def prepare(self, owner, timeout=300):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            before = self.pods("worker")
            if not before or any(p["metadata"].get("deletionTimestamp") or not p.get("status", {}).get("podIP") or
                    not any(c["type"] == "Ready" and c["status"] == "True" for c in p.get("status", {}).get("conditions", []))
                    for p in before.values()):
                time.sleep(0.5)
                continue
            try:
                self.call(self.control_plane(), "prepare", owner,
                          ",".join(sorted(p["status"]["podIP"] for p in before.values())),
                          timeout=min(250, max(1, deadline - time.monotonic())))
                if self.identity(before) == self.identity(self.pods("worker")):
                    return
            except subprocess.CalledProcessError:
                # Preparation is idempotent; allow stale Service DNS to converge.
                pass
            time.sleep(0.5)
        raise ValueError("Active applications could not be prepared; admission remains closed. Retry start.")
