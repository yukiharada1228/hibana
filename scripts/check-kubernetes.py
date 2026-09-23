#!/usr/bin/env python3
"""Check the rendered deployment's safety contracts, offline. Requires kubectl + PyYAML."""
import subprocess
import base64
import json
import tempfile
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
    assert not named(docs, "Service", "hibana-worker-discovery")["spec"].get("publishNotReadyAddresses", False)
    assert config["WORKER_PREPARATION_URL"] == "http://hibana-worker-preparation:8084"
    preparation = named(docs, "Service", "hibana-worker-preparation")["spec"]
    assert preparation["clusterIP"] == "None" and preparation["publishNotReadyAddresses"] is True
    assert preparation["selector"] == named(docs, "Service", "hibana-worker-discovery")["spec"]["selector"]
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
    if overlay == "remote":
        assert named(docs, "NetworkPolicy", "hibana-dependencies")["spec"]["podSelector"]["matchExpressions"] == [{"key": "app.kubernetes.io/name", "operator": "NotIn", "values": ["hibana-console"]}]
        console = named(docs, "Deployment", "hibana-console")["spec"]["template"]
        assert console["metadata"]["labels"]["hibana.io/api-client"] == "true"
        pod = console["spec"]
        assert pod["automountServiceAccountToken"] is False
        assert pod["securityContext"]["runAsNonRoot"] is True
        container = pod["containers"][0]
        assert container["securityContext"]["readOnlyRootFilesystem"] is True
        assert "envFrom" not in container, "console must not inherit database or platform credentials"
        assert container["env"] == [{"name": "HIBANA_API_UPSTREAM", "value": "http://hibana-api:8080"}]
        policy = named(docs, "NetworkPolicy", "hibana-console")["spec"]
        assert policy["egress"] == [{"to": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "hibana-control-plane"}}}], "ports": [{"protocol": "TCP", "port": 8080}]}]
        ingress = named(docs, "Ingress", "hibana-console")["spec"]
        assert ingress["tls"] and ingress["rules"][0]["http"]["paths"][0]["backend"]["service"]["name"] == "hibana-console"

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

# Render the same independent identity overlay produced by the distributable CLI.
# Keep generated credentials and rendered Secrets out of test output and Git.
subprocess.run(["node", "sdk/scripts/pack.mjs"], cwd=root, check=True)
with tempfile.TemporaryDirectory(prefix="hibana-identity-manifests-") as temporary:
    site = Path(temporary) / "site"
    subprocess.run([
        "node", "--input-type=module", "-e",
        "import { initPlatform } from './sdk/src/platform-init.mjs'; "
        "await initPlatform(process.argv[1], {withKeycloak: true});", str(site),
    ], cwd=root, check=True, stdout=subprocess.DEVNULL)
    docs = render(site / "identity")
    assert named(docs, "Namespace", "hibana-identity")
    assert all(d["metadata"].get("namespace") == "hibana-identity"
               for d in docs if d["kind"] != "Namespace")
    platform = render(site)
    assert not any(d["metadata"].get("namespace") == "hibana-identity" for d in platform)
    secrets = {d["metadata"]["name"]: d["data"] for d in docs if d["kind"] == "Secret"}
    deployment = named(docs, "Deployment", "keycloak")["spec"]
    assert deployment["replicas"] == 1 and deployment["strategy"]["type"] == "Recreate"
    pod = deployment["template"]["spec"]
    assert pod["automountServiceAccountToken"] is False
    assert pod["securityContext"]["runAsNonRoot"] is True
    assert pod["securityContext"]["seccompProfile"]["type"] == "RuntimeDefault"
    container = pod["containers"][0]
    assert container["image"] == "quay.io/keycloak/keycloak:26.7.4"
    assert container["args"] == ["start", "--import-realm"]
    assert container["securityContext"]["allowPrivilegeEscalation"] is False
    assert container["securityContext"]["capabilities"]["drop"] == ["ALL"]
    for probe, path in [("startupProbe", "started"), ("readinessProbe", "ready"), ("livenessProbe", "live")]:
        assert container[probe]["httpGet"] == {"path": f"/health/{path}", "port": "health"}
    for env in container["envFrom"]:
        if "secretRef" in env:
            assert env["secretRef"]["name"] in secrets
    database_ref = container["env"][0]["valueFrom"]["secretKeyRef"]
    assert database_ref["key"] in secrets[database_ref["name"]]
    realm_secret = secrets[pod["volumes"][0]["secret"]["secretName"]]
    realm = json.loads(base64.b64decode(realm_secret["hibana-realm.json"]))
    assert realm["realm"] == "hibana" and realm["sslRequired"] == "all"
    assert not realm.get("users") and realm["registrationAllowed"] is False
    client = realm["clients"][0]
    assert client["standardFlowEnabled"] is True
    assert all(client[key] is False for key in ["publicClient", "directAccessGrantsEnabled", "implicitFlowEnabled", "serviceAccountsEnabled"])
    assert client["attributes"]["pkce.code.challenge.method"] == "S256"
    assert client["redirectUris"] == ["${HIBANA_OIDC_CALLBACK_URL}"]
    assert client["secret"] == "${HIBANA_OIDC_CLIENT_SECRET}"
    client_data = next(data for data in secrets.values() if "HIBANA_OIDC_CLIENT_SECRET" in data)
    shared_secret = base64.b64decode(client_data["HIBANA_OIDC_CLIENT_SECRET"]).decode()
    assert len(shared_secret) == 64
    assert f"OIDC_CLIENT_SECRET={shared_secret}\n" in (site / "control-plane.env").read_text()
    config = named(docs, "ConfigMap", "keycloak-config")["data"]
    hibana_config = named(platform, "ConfigMap", "hibana-config")["data"]
    assert config["KC_PROXY_HEADERS"] == "xforwarded"
    assert config["KC_PROXY_TRUSTED_ADDRESSES"] == "CHANGE_ME_INGRESS_PROXY_CIDRS"
    assert hibana_config["OIDC_ISSUER_URL"] == config["KC_HOSTNAME"] + "/realms/hibana"
    assert hibana_config["OIDC_CALLBACK_URL"] == config["HIBANA_OIDC_CALLBACK_URL"]
    assert hibana_config["OIDC_CONSOLE_URL"] == config["HIBANA_OIDC_CONSOLE_URL"]
    db = named(docs, "Deployment", "keycloak-postgres")["spec"]
    assert db["replicas"] == 1 and db["strategy"]["type"] == "Recreate"
    db_pod = db["template"]["spec"]
    assert db_pod["automountServiceAccountToken"] is False
    assert db_pod["containers"][0]["envFrom"][0]["secretRef"]["name"] == database_ref["name"]
    claim = db_pod["volumes"][0]["persistentVolumeClaim"]["claimName"]
    assert named(docs, "PersistentVolumeClaim", claim)["spec"]["resources"]["requests"]["storage"] == "2Gi"
    for service in (d for d in docs if d["kind"] == "Service"):
        assert service["spec"].get("type", "ClusterIP") == "ClusterIP"
        assert all(p["port"] != 9000 for p in service["spec"]["ports"])
    ingress = named(docs, "Ingress", "keycloak")["spec"]
    host = config["KC_HOSTNAME"].removeprefix("https://")
    assert ingress["tls"][0]["hosts"] == [host]
    assert ingress["rules"][0]["host"] == host
    assert ingress["rules"][0]["http"]["paths"][0]["backend"]["service"] == {"name": "keycloak", "port": {"number": 8080}}
    deny = named(docs, "NetworkPolicy", "identity-default-deny")["spec"]
    assert deny["podSelector"] == {} and set(deny["policyTypes"]) == {"Ingress", "Egress"}
    access = named(docs, "NetworkPolicy", "keycloak")["spec"]
    assert access["ingress"] == [{"from": [{"namespaceSelector": {"matchLabels": {"hibana.io/identity-ingress": "true"}}}], "ports": [{"protocol": "TCP", "port": 8080}]}]
    database = named(docs, "NetworkPolicy", "keycloak-postgres")["spec"]
    assert database["ingress"] == [{"from": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "keycloak"}}}], "ports": [{"protocol": "TCP", "port": 5432}]}]
    assert database["egress"] == []
print("identity: generated overlay renders with independent namespace, persistent DB, private credentials and HTTPS/PKCE contracts")
