"""Close fleet admission and drain through CP-local APIs; no credentials leave Pods."""
import json
import subprocess
import time


# Do not infer legacy support from kubectl's exit code: both an exec transport
# error and grep's "no match" can be exit 1. Only successful inspection may opt
# into the legacy lifecycle; an unfamiliar maintenance implementation must stop.
PROTOCOL_PROBE = '''
grep -aFq -- "$1" "$2"
case $? in
    0) printf supported; exit 0;;
    1) ;;
    *) exit 2;;
esac
grep -aFq -- --maintenance "$2"
case $? in
    0) printf incompatible;;
    1) printf legacy;;
    *) exit 2;;
esac
'''


class NoControlPlane(ValueError):
    """No CP container can currently serve an operator request."""


class Maintenance:
    def __init__(self, target):
        self.target = target

    def pods(self, component):
        items = json.loads(self.target.kube("-n", "hibana", "get", "pods", "-l",
            f"app.kubernetes.io/name=hibana-{component}", "-o", "json", capture=True, timeout=30))["items"]
        return {p["metadata"]["uid"]: p for p in items}

    def supports_protocol(self):
        """Inspect legacy images without executing their server with unknown flags.

        The original binary ignores --maintenance and starts a second server.
        Its successor embeds this command's usage text. Inspect it inside the
        container: image tags are mutable and all candidate Pods must agree.
        A different maintenance implementation is incompatible, not legacy.
        """
        before = self.pods("control-plane")
        supported = set()
        for pod in before.values():
            if not any(c.get("name") == "control-plane" and "running" in c.get("state", {})
                       for c in pod.get("status", {}).get("containerStatuses", [])):
                continue
            result = self.target.kube("-n", "hibana", "exec", f"pod/{pod['metadata']['name']}",
                "-c", "control-plane", "--", "sh", "-c", PROTOCOL_PROBE,
                "hibana-maintenance-probe",
                "Usage: --maintenance close|open OWNER | status | prepare OWNER WORKER_IPS",
                "/usr/local/bin/hibana-control-plane", capture=True, quiet=True, timeout=15)
            if result not in ("supported", "legacy"):
                raise ValueError("Could not determine Control Plane maintenance compatibility; maintenance was not attempted")
            supported.add(result == "supported")
        if not supported:
            raise NoControlPlane("No running Control Plane; maintenance compatibility is unknown")
        if len(supported) != 1 or self.identity(before) != self.identity(self.pods("control-plane")):
            raise ValueError("Control Plane changed or mixes maintenance protocols; finish the rollout and retry")
        return supported.pop()

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

    def prepare(self, owner=None, timeout=300):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            before = self.pods("worker")
            if not before or any(p["metadata"].get("deletionTimestamp") or not p.get("status", {}).get("podIP") or
                    not any(c["type"] == "Ready" and c["status"] == "True" for c in p.get("status", {}).get("conditions", []))
                    for p in before.values()):
                time.sleep(0.5)
                continue
            try:
                self.call(self.control_plane(), "prepare", owner or "",
                          ",".join(sorted(p["status"]["podIP"] for p in before.values())),
                          timeout=min(250, max(1, deadline - time.monotonic())))
                if self.identity(before) == self.identity(self.pods("worker")):
                    return
            except subprocess.CalledProcessError:
                # Preparation is idempotent; allow stale Service DNS to converge.
                pass
            time.sleep(0.5)
        if owner:
            raise ValueError("Active applications could not be prepared; admission remains closed. Retry start.")
        raise ValueError("Active applications could not be prepared; installation is incomplete. Fix preparation and retry install.")
