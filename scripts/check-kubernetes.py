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
]:
    docs = render(f"deploy/kubernetes/{overlay}")
    config = named(docs, "ConfigMap", "hibana-config")["data"]
    assert config["RUN_MIGRATIONS"] == "false"
    assert not any(key.startswith(("NATS_", "JETSTREAM_", "LANE_", "SCALE_")) for key in config)
    assert all(isinstance(v, str) for v in config.values())
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
            assert refs == {"hibana-runtime"}, "worker must not inherit signing, S3 or encryption credentials"
            assert 0 < int(config["WORKER_DRAIN_TIMEOUT_SECS"])
            assert pod["terminationGracePeriodSeconds"] > int(config["WORKER_DRAIN_TIMEOUT_SECS"])
        assert named(docs, "PodDisruptionBudget", f"hibana-{component}")["spec"]["minAvailable"] == 1
    assert all(d["spec"].get("type", "ClusterIP") == "ClusterIP" for d in docs if d["kind"] == "Service")
    deny = named(docs, "NetworkPolicy", "default-deny")["spec"]
    assert deny["podSelector"] == {} and set(deny["policyTypes"]) == {"Ingress", "Egress"}
    internal = named(docs, "NetworkPolicy", "hibana-internal")["spec"]["ingress"][0]
    assert internal["ports"] == [{"protocol": "TCP", "port": 8081}]
    assert internal["from"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/name"] == "hibana-worker"
    print(f"{overlay}: deployment contracts passed ({len(docs)} resources)")

for path in ["migration", "local/migration"]:
    docs = render(f"deploy/kubernetes/{path}")
    pod = named(docs, "Job", "hibana-migrate")["spec"]["template"]["spec"]
    container = pod["containers"][0]
    assert pod["automountServiceAccountToken"] is False
    assert container["command"] == ["/usr/local/bin/control-plane", "--migrate-only"]
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
