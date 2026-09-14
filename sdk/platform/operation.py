"""Serialize operators through a durable, non-expiring Kubernetes ConfigMap."""
from copy import deepcopy
from datetime import datetime, timezone
from functools import wraps
import json
import shlex
import subprocess
import sys
import uuid

NAME = "hibana-platform-operation"
MANAGER = "app.kubernetes.io/managed-by"


def exclusive(method):
    @wraps(method)
    def run(self, *args, **kwargs):
        with self.operation(method.__name__):
            return method(self, *args, **kwargs)
    return run


class OperationLock:
    """Atomic create/replace; no TTL can release a still-running operator.

    The acquired resourceVersion also fences release: an interrupted old owner
    must never clear a lock that an administrator has recovered and re-acquired.
    A killed operator requires explicit recovery after its process has stopped.
    """
    def __init__(self, target, action):
        self.target, self.action = target, action
        self.acquired = None

    def __enter__(self):
        # In particular, check release permission before creating a lock.
        for verb, resource in (("get", f"configmaps/{NAME}"), ("create", "configmaps"),
                               ("update", f"configmaps/{NAME}")):
            try:
                allowed = self.target.kube("-n", "hibana", "auth", "can-i", verb, resource,
                                           capture=True, quiet=True, timeout=20)
            except subprocess.CalledProcessError:
                allowed = "no"
            if allowed.strip() != "yes":
                raise ValueError(f"Kubernetes permission is missing for the operation lock: {verb} {resource}")
        current = self.target.get("configmap", NAME)
        if current:
            if current["metadata"].get("labels", {}).get(MANAGER) != "hibana":
                raise ValueError(f"ConfigMap/{NAME} belongs to another manager")
            if "owner" not in current.get("data", {}):
                raise ValueError(f"Invalid operation lock: ConfigMap/{NAME}")
            if current["data"]["owner"]:
                data = current["data"]
                inspect = shlex.join([*self.target.kubectl, "-n", "hibana", "get", "configmap", NAME, "-o", "yaml"])
                raise ValueError(f"Another platform operation holds the lock: {data.get('action', 'unknown')} "
                    f"(owner {data['owner']}, started {data.get('started_at', 'unknown')}). "
                    f"Wait for it to finish. Inspect: {inspect}. If its CLI was forcibly terminated, "
                    f"confirm the old process has stopped before deleting ConfigMap/{NAME} and retrying.")
        candidate = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {
            "name": NAME, "namespace": "hibana", "labels": {MANAGER: "hibana"}}, "data": {
            "owner": uuid.uuid4().hex, "action": self.action,
            "started_at": datetime.now(timezone.utc).isoformat()}}
        if current:
            candidate["metadata"].update({key: current["metadata"][key] for key in ("uid", "resourceVersion")})
        try:
            raw = self.target.kube("create" if current is None else "replace", "-f", "-", "-o", "json",
                input=json.dumps(candidate), capture=True, quiet=True, timeout=30)
            self.acquired = json.loads(raw)
        except subprocess.SubprocessError as error:
            # Never continue after a conflict or ambiguous API response. A lost
            # successful response leaves a held lock, requiring inspection.
            raise ValueError(f"Could not acquire platform operation lock ConfigMap/{NAME}. "
                             "Another operator may have acquired it; inspect the lock before retrying.") from error
        return self

    def __exit__(self, error_type, error, traceback):
        candidate = deepcopy(self.acquired)
        candidate["data"] = {"owner": ""}
        try:
            self.target.kube("replace", "-f", "-", "-o", "json", input=json.dumps(candidate),
                             capture=True, quiet=True, timeout=30)
        except (OSError, subprocess.SubprocessError) as release_error:
            message = f"Could not release ConfigMap/{NAME}; inspect the operation lock before retrying."
            if error_type is None:
                raise ValueError(message) from release_error
            print(message, file=sys.stderr)
        return False
