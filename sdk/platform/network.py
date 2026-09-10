"""Add desired network access during rollout without removing existing access."""
from copy import deepcopy
import hashlib
import json
import subprocess

PREFIX = "hibana-install-network-"
MANAGER = "app.kubernetes.io/managed-by"


def directions(spec):
    return spec.get("policyTypes") or ["Ingress"] + (["Egress"] if spec.get("egress") else [])


def intersection(*selectors):
    """Intersect selectors, returning None for an impossible combination."""
    requirements = []
    for selector in selectors:
        requirements.extend(selector.get("matchExpressions", []))
        requirements.extend({"key": k, "operator": "In", "values": [v]}
                            for k, v in selector.get("matchLabels", {}).items())
    combined = []
    for key in sorted({r["key"] for r in requirements}):
        allowed, forbidden, present, absent = None, set(), False, False
        for rule in (r for r in requirements if r["key"] == key):
            operator, values = rule["operator"], set(rule.get("values", []))
            if operator == "In":
                allowed = values if allowed is None else allowed & values
                present = True
            elif operator == "NotIn":
                forbidden.update(values)
            elif operator == "Exists":
                present = True
            elif operator == "DoesNotExist":
                absent = True
            else:
                raise ValueError(f"Unsupported NetworkPolicy selector operator: {operator}")
        if (absent and present) or (allowed is not None and not (allowed - forbidden)):
            return None
        if absent:
            combined.append({"key": key, "operator": "DoesNotExist"})
        elif allowed is not None:
            combined.append({"key": key, "operator": "In", "values": sorted(allowed - forbidden)})
        else:
            if present:
                combined.append({"key": key, "operator": "Exists"})
            if forbidden:
                combined.append({"key": key, "operator": "NotIn", "values": sorted(forbidden)})
    return {"matchExpressions": combined} if combined else {}


def outside(selector, others):
    """Parts of selector outside the union of others (formerly isolated Pods)."""
    remaining = [intersection(selector)]
    inverses = {"In": "NotIn", "NotIn": "In", "Exists": "DoesNotExist", "DoesNotExist": "Exists"}
    # Handle broad selectors first to avoid expanding exclusions unnecessarily.
    for other in sorted((intersection(s) for s in others), key=lambda s: len((s or {}).get("matchExpressions", []))):
        if other is None:
            continue
        alternatives = [{**r, "operator": inverses[r["operator"]]} for r in other.get("matchExpressions", [])]
        parts = {json.dumps(part, sort_keys=True) for current in remaining if current is not None
                 for rule in alternatives if (part := intersection(current, {"matchExpressions": [rule]})) is not None}
        remaining = [json.loads(part) for part in sorted(parts)]
        if len(remaining) > 256:
            raise ValueError("NetworkPolicy selector transition is too complex. Split selector changes into smaller installation updates.")
        if not remaining:
            break
    return [part for part in remaining if part is not None]


class NetworkTransition:
    def __init__(self, target, docs, record, namespace_exists, *, preserve_access=True):
        self.target = target
        self.desired = [d for d in docs if d["kind"] == "NetworkPolicy"]
        self.cleanup = {r["name"] for r in record["resources"]
                        if r["kind"] == "NetworkPolicy" and r["name"].startswith(PREFIX)}
        self.current = {}
        if namespace_exists and (self.desired or self.cleanup):
            try:
                raw = target.kube("-n", "hibana", "get", "networkpolicies", "-o", "json",
                                  capture=True, quiet=True, timeout=20)
            except subprocess.CalledProcessError as error:
                raise ValueError("Cannot inspect NetworkPolicies in namespace hibana. Check cluster access and list permission for networkpolicies.networking.k8s.io.") from error
            self.current = {d["metadata"]["name"]: d for d in json.loads(raw)["items"]}
        for name, doc in self.current.items():
            if name.startswith(PREFIX) and (name not in self.cleanup or doc["metadata"].get("labels", {}).get(MANAGER) != "hibana"):
                raise ValueError(f"Temporary NetworkPolicy/{name} is not owned by this installation")
        policies = [d for n, d in self.current.items() if not n.startswith(PREFIX)]
        temporary = {}
        def allow(selector, direction, rules):
            if selector is None:
                return
            addition = {"podSelector": selector, "policyTypes": [direction], direction.lower(): deepcopy(rules)}
            name = PREFIX + hashlib.sha256(json.dumps(addition, sort_keys=True).encode()).hexdigest()[:24]
            temporary[name] = {"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                "metadata": {"name": name, "namespace": "hibana", "labels": {MANAGER: "hibana"}}, "spec": addition}

        for desired in self.desired:
            spec = desired["spec"]
            before = self.current.get(desired["metadata"]["name"], {}).get("spec")
            if before == spec:
                continue
            for direction in directions(spec):
                rules = spec.get(direction.lower(), [])
                if not rules:
                    continue
                for old in policies:
                    if direction not in directions(old["spec"]):
                        continue
                    # Only select Pods already isolated in this direction. Pods
                    # outside these intersections already allow all traffic;
                    # selecting them here would unexpectedly isolate them.
                    selector = intersection(spec["podSelector"], old["spec"]["podSelector"])
                    allow(selector, direction, rules)
        final = {d["metadata"]["name"]: d for d in policies + self.desired}
        for direction in ("Ingress", "Egress"):
            selected = [d["spec"]["podSelector"] for d in final.values() if direction in directions(d["spec"])]
            for old in policies:
                if direction in directions(old["spec"]):
                    # Removing isolation is also a grant of access. Preserve its
                    # exact scope when selectors or policyTypes are changed.
                    for selector in outside(old["spec"]["podSelector"], selected):
                        allow(selector, direction, [{}])
        self.temporary = list(temporary.values())
        self.cleanup.update(temporary)
        # Pre-created namespaces also need isolation before their first Pods.
        self.before_migration = self.temporary if namespace_exists and preserve_access else self.desired

    def finish(self):
        if self.desired:
            self.target.apply(self.desired)
        for name in sorted(self.cleanup):
            current = self.target.get("NetworkPolicy", name)
            if current and current["metadata"].get("labels", {}).get(MANAGER) != "hibana":
                raise ValueError(f"Resource ownership changed: NetworkPolicy/{name}")
            self.target.kube("-n", "hibana", "delete", "NetworkPolicy", name, "--ignore-not-found")
