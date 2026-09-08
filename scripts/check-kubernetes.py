#!/usr/bin/env python3
"""Check the rendered deployment's safety contracts, offline. Requires kubectl + PyYAML."""
import subprocess
from pathlib import Path
import yaml

root = Path(__file__).resolve().parents[1]
def render(path):
    text = subprocess.check_output(["kubectl", "kustomize", str(root / path)], text=True)
    return [d for d in yaml.safe_load_all(text) if d]
def named(docs, kind, name):
    return next(d for d in docs if d["kind"] == kind and d["metadata"]["name"] == name)

for overlay, image in [
    ("base", "registry.example.com/hibana/platform:replace-with-release"),
    ("local", "hibana-platform:dev"),
    ("remote", "registry.example.com/hibana/platform:replace-with-release"),
]:
    docs = render(f"deploy/kubernetes/{overlay}")
    config = named(docs, "ConfigMap", "hibana-config")["data"]
    assert config["RUN_MIGRATIONS"] == "false"
    assert not any(key.startswith(("NATS_", "JETSTREAM_", "LANE_", "SCALE_")) for key in config)
    assert all(isinstance(v, str) for v in config.values())
    assert config["WORKER_HTTP_URL"] == "http://hibana-worker-discovery:8084"
    assert named(docs, "Service", "hibana-worker-discovery")["spec"]["clusterIP"] == "None"
    assert int(config["WORKER_MAX_COMPILATIONS"]) == 1
    assert int(config["WORKER_DB_MAX_CONNECTIONS"]) == 8
    for component in ["control-plane", "worker"]:
        deployment = named(docs, "Deployment", f"hibana-{component}")
        spec = deployment["spec"]
        pod = spec["template"]["spec"]
        container = pod["containers"][0]
        assert spec["replicas"] >= 2
        assert spec["strategy"]["rollingUpdate"]["maxUnavailable"] == 0
        assert pod["automountServiceAccountToken"] is False
        assert pod["securityContext"]["runAsNonRoot"] is True
        assert pod["securityContext"]["seccompProfile"]["type"] == "RuntimeDefault"
        assert container["image"] == image
        assert container["securityContext"]["readOnlyRootFilesystem"] is True
        assert container["securityContext"]["allowPrivilegeEscalation"] is False
        assert "ALL" in container["securityContext"]["capabilities"]["drop"]
        assert container["readinessProbe"]["httpGet"]["path"] == "/readyz"
        assert container["livenessProbe"]["httpGet"]["path"] == "/healthz"
        assert pod["topologySpreadConstraints"][0]["minDomains"] >= 2
        assert pod["topologySpreadConstraints"][0]["nodeTaintsPolicy"] == "Honor"
        assert pod["topologySpreadConstraints"][0]["matchLabelKeys"] == ["pod-template-hash"]
        refs = {v["secretRef"]["name"] for v in container["envFrom"] if "secretRef" in v}
        assert "hibana-migration" not in refs
        if component == "worker":
            assert int(config["WORKER_GUEST_MEMORY_BUDGET_MIB"]) == 2048
            assert container["resources"]["limits"]["memory"] == "4Gi", "leave non-guest memory headroom"
            assert refs == {"hibana-runtime"}, "worker must not inherit signing, S3 or encryption credentials"
            assert 0 < int(config["WORKER_DRAIN_TIMEOUT_SECS"])
            assert pod["terminationGracePeriodSeconds"] > int(config["WORKER_DRAIN_TIMEOUT_SECS"])
        assert named(docs, "PodDisruptionBudget", f"hibana-{component}")["spec"]["minAvailable"] == 1
    published = {"hibana-api": 30080, "hibana-apps": 30083} if overlay == "local" else {}
    for service in (d for d in docs if d["kind"] == "Service"):
        name, spec = service["metadata"]["name"], service["spec"]
        if name in published:
            assert spec["type"] == "NodePort"
            assert spec["ports"][0]["nodePort"] == published[name]
        else:
            assert spec.get("type", "ClusterIP") == "ClusterIP", "internal services must stay private"
    if overlay == "local":
        access = named(docs, "NetworkPolicy", "hibana-local-access")["spec"]
        assert access["podSelector"]["matchLabels"]["app.kubernetes.io/name"] == "hibana-control-plane"
        assert access["ingress"] == [{"ports": [{"protocol": "TCP", "port": 8080}, {"protocol": "TCP", "port": 8083}]}]
    else:
        assert not any(d["metadata"]["name"] == "hibana-local-access" for d in docs)
    deny = named(docs, "NetworkPolicy", "default-deny")["spec"]
    assert deny["podSelector"] == {} and set(deny["policyTypes"]) == {"Ingress", "Egress"}
    internal = named(docs, "NetworkPolicy", "hibana-internal")["spec"]["ingress"][0]
    assert internal["ports"] == [{"protocol": "TCP", "port": 8081}]
    assert internal["from"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/name"] == "hibana-worker"
    print(f"{overlay}: deployment contracts passed ({len(docs)} resources)")

kind = yaml.safe_load((root / "deploy/kubernetes/local/kind.yaml").read_text())
ports = kind["nodes"][0]["extraPortMappings"]
assert {(p["containerPort"], p["hostPort"]) for p in ports} == {(30080, 18080), (30083, 18084)}
assert all(p["listenAddress"] == "127.0.0.1" and p["protocol"] == "TCP" for p in ports)
print("local access: API/apps use loopback-bound kind ports; no forwarding process required")

for path in ["migration", "local/migration", "remote/migration"]:
    docs = render(f"deploy/kubernetes/{path}")
    pod = named(docs, "Job", "hibana-migrate")["spec"]["template"]["spec"]
    container = pod["containers"][0]
    assert pod["automountServiceAccountToken"] is False
    assert container["command"] == ["/usr/local/bin/hibana-control-plane", "--migrate-only"]
    assert "envFrom" not in container
    assert {v["name"] for v in container["env"]} == {"MIGRATION_DATABASE_URL", "LOG_FORMAT"}
    print(f"{path}: isolated migration credentials passed")

assert not any("nats" in d["metadata"]["name"] for d in render("deploy/kubernetes/local/dependencies"))
print("local dependencies: render passed (ephemeral, not HA)")

for component in ["worker"]:
    docs = render("deploy/kubernetes/hardened")
    pod = named(docs, "Deployment", "hibana-" + component)["spec"]["template"]["spec"]
    assert pod["runtimeClassName"] == "hibana-sandbox"
    assert pod["nodeSelector"] == {"hibana.io/untrusted-code": "true"}
print("hardened: explicit sandbox RuntimeClass and untrusted-code node pool required")
docs = render("deploy/kubernetes/persistent-dependencies")
for component in ["postgres", "minio"]:
    pod = named(docs, "Deployment", "hibana-" + component)["spec"]["template"]["spec"]
    data = next(v for v in pod["volumes"] if v["name"] == "data")
    assert "emptyDir" not in data
    claim = data["persistentVolumeClaim"]["claimName"]
    assert named(docs, "PersistentVolumeClaim", claim)["spec"]["resources"]["requests"]["storage"]
print("persistent dependencies: PostgreSQL/MinIO use PVCs (single replica, not HA)")

docs = render("deploy/kubernetes/autoscaling")
assert len(docs) == 1, "HPA add-on must not reapply Deployment replicas"
hpa = named(docs, "HorizontalPodAutoscaler", "hibana-worker")
assert hpa["apiVersion"] == "autoscaling/v2"
spec = hpa["spec"]
assert spec["scaleTargetRef"]["name"] == "hibana-worker"
assert spec["minReplicas"] == 2 and spec["maxReplicas"] == 8
assert spec["metrics"][0]["resource"]["target"]["averageUtilization"] == 70
assert spec["behavior"]["scaleDown"]["stabilizationWindowSeconds"] >= 300
print("autoscaling: optional Worker HPA, bounded growth and stabilized scale-down")

# The management API and tenant app hosts must reach different public listeners.
remote = render("deploy/kubernetes/remote")
for name, service, port, host in [
    ("hibana-management", "hibana-api", 8080, "api.example.internal"),
    ("hibana-apps-team", "hibana-apps", 8083, "*.team.apps.example.internal"),
]:
    spec = named(remote, "Ingress", name)["spec"]
    assert spec["tls"][0]["hosts"] == [host]
    assert spec["rules"][0]["host"] == host
    backend = spec["rules"][0]["http"]["paths"][0]["backend"]["service"]
    assert backend == {"name": service, "port": {"number": port}}
print("remote ingress: management/app TLS hosts separated; no internal Worker endpoint exposed")
