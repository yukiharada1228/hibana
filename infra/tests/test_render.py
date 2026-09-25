"""Check the installer's real manifest contract, retention, identity and proxy trust."""

import base64
import json
import shlex
import stat
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "infra"))
from render import render

sys.path.insert(0, str(ROOT / "sdk/platform"))
from existing import validate


class RenderTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        self.cfg = yaml.safe_load((ROOT / "infra/site.example.yml").read_text())[
            "hibana"
        ]
        self.sec = {
            k: "ab" * 32
            for k in [
                "db_owner",
                "db_app",
                "redis",
                "s3",
                "oidc",
                "bootstrap",
                "signing",
                "master",
                "keycloak_db",
                "keycloak_admin",
                "owner_password",
            ]
        }
        self.sec.update(
            owner_subject=str(uuid.uuid4()), cloudflare_dns_token="fixture-token"
        )

    def tearDown(self):
        self.tmp.cleanup()

    def docs(self, sub):
        p = subprocess.run(
            ["kubectl", "kustomize", str(self.out / sub)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(p.returncode, 0, p.stderr)
        return list(yaml.safe_load_all(p.stdout))

    def test_install_contract_retention_and_trusted_proxies(self):
        render(self.out, self.cfg, self.sec)
        site = self.docs("site")
        validate(site + self.docs("site/migration"))
        runtime = next(d for d in site if d["kind"] == "Secret" and d["metadata"]["name"] == "hibana-runtime")
        key = base64.b64decode(runtime["data"]["COMPILED_CACHE_KEY"]).decode()
        from kubernetes import compiled_cache_key
        self.assertEqual(key, compiled_cache_key(self.sec["signing"]))
        self.assertNotEqual(key, self.sec["signing"])
        deps = self.docs("dependencies")
        self.assertFalse(any("minio" in d["metadata"]["name"] for d in deps))
        garage = next(
            d
            for d in deps
            if d["kind"] == "Deployment" and d["metadata"]["name"] == "hibana-objects"
        )
        self.assertIn(
            "@sha256:", garage["spec"]["template"]["spec"]["containers"][0]["image"]
        )
        for d in deps:
            if d["kind"] == "Deployment":
                pod = d["spec"]["template"]["spec"]
                self.assertTrue(pod["securityContext"]["runAsNonRoot"])
                self.assertTrue(pod["securityContext"]["fsGroup"])
                self.assertEqual(
                    pod["securityContext"]["seccompProfile"]["type"], "RuntimeDefault"
                )
                for container in pod["containers"]:
                    self.assertFalse(
                        container["securityContext"]["allowPrivilegeEscalation"]
                    )
                    self.assertEqual(
                        container["securityContext"]["capabilities"]["drop"], ["ALL"]
                    )
        worker = next(d for d in site if d["kind"] == "Deployment" and d["metadata"]["name"] == "hibana-worker")
        self.assertNotIn("lifecycle", worker["spec"]["template"]["spec"]["containers"][0])
        identity = self.docs("identity")
        infrastructure = self.docs("infrastructure")
        for d in deps + identity:
            if d["kind"] == "PersistentVolumeClaim":
                self.assertEqual(d["spec"]["storageClassName"], "hibana-local")
                self.assertTrue(d["spec"]["volumeName"])
            if d["kind"] == "Deployment":
                self.assertEqual(d["spec"]["replicas"], 1)
                self.assertEqual(d["spec"]["strategy"], {"type": "Recreate"})
        pvs = [d for d in infrastructure if d["kind"] == "PersistentVolume"]
        self.assertEqual(len(pvs), 4)
        self.assertTrue(
            all(d["spec"]["persistentVolumeReclaimPolicy"] == "Retain" for d in pvs)
        )
        cfg = next(
            d["data"]
            for d in site
            if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "hibana-config"
        )
        self.assertEqual(
            cfg["TRUSTED_PROXY_CIDRS"], "10.244.255.10/32,10.244.255.11/32"
        )
        self.assertEqual(cfg["WORKER_GUEST_MEMORY_BUDGET_MIB"], "1024")
        self.assertEqual(cfg["S3_ENDPOINT"], "http://hibana-objects:9000")
        realm = json.loads(
            (self.out / "identity/imports/hibana-realm.json").read_text()
        )
        self.assertEqual(realm["users"][0]["requiredActions"], ["UPDATE_PASSWORD"])
        self.assertEqual(realm["accessCodeLifespanUserAction"], 900)
        self.assertEqual(
            realm["users"][0]["clientRoles"],
            {"account": ["manage-account", "view-profile"]},
        )
        apps = next(
            d
            for d in site
            if d["kind"] == "Ingress" and d["metadata"]["name"] == "hibana-apps"
        )
        self.assertEqual(apps["spec"]["rules"][0]["host"], "*.local.apps.hibana.cloud")
        for d in site:
            if d["kind"] == "Deployment":
                self.assertFalse(
                    d["spec"]["template"]["spec"].get("topologySpreadConstraints")
                )
                self.assertNotIn("replace-with-release", json.dumps(d))

    def test_acme_startup_preserves_keys_and_repairs_volume_permissions(self):
        render(self.out, self.cfg, self.sec)
        traefik = next(
            d for d in self.docs("infrastructure")
            if d["kind"] == "Deployment" and d["metadata"]["name"] == "traefik"
        )["spec"]["template"]["spec"]
        self.assertEqual(traefik["securityContext"]["runAsUser"], 65532)
        init = traefik["initContainers"][0]
        self.assertEqual(init["image"], traefik["containers"][0]["image"])
        self.assertFalse(init["securityContext"]["allowPrivilegeEscalation"])
        acme = self.out / "acme.json"
        command = [s.replace("/data/acme.json", shlex.quote(str(acme))) for s in init["command"]]
        subprocess.run(command, check=True)
        self.assertEqual(stat.S_IMODE(acme.stat().st_mode), 0o600)
        acme.write_text('{"fixture": "preserved certificate data"}')
        for _ in range(2):
            acme.chmod(0o660)  # kubelet reapplies fsGroup on a volume mount
            subprocess.run(command, check=True)
            self.assertEqual(stat.S_IMODE(acme.stat().st_mode), 0o600)
            self.assertEqual(acme.read_text(), '{"fixture": "preserved certificate data"}')

    def test_rerender_preserves_secrets_and_rejects_accidental_rotation(self):
        render(self.out, self.cfg, self.sec)
        before = {
            p.relative_to(self.out): p.read_bytes()
            for p in self.out.rglob("*")
            if p.is_file()
        }
        render(self.out, self.cfg, self.sec)
        after = {
            p.relative_to(self.out): p.read_bytes()
            for p in self.out.rglob("*")
            if p.is_file()
        }
        self.assertEqual(before, after)
        self.sec["db_app"] = "cd" * 32
        with self.assertRaisesRegex(ValueError, "Persistent identity"):
            render(self.out, self.cfg, self.sec)

    def test_reject_invalid_network_and_allow_dns_token_rotation(self):
        render(self.out, self.cfg, self.sec)
        self.sec["cloudflare_dns_token"] = "rotated-fixture-token"
        render(self.out, self.cfg, self.sec)
        self.cfg["console_pod_ip"] = self.cfg["ingress_pod_ip"]
        with self.assertRaisesRegex(ValueError, "distinct"):
            render(self.out, self.cfg, self.sec)

    def test_provided_tls_reaches_oidc_client(self):
        self.cfg["tls_mode"] = "provided"
        self.sec.update(
            tls_crt="fixture certificate", tls_key="fixture key", ca_crt="fixture CA"
        )
        render(self.out, self.cfg, self.sec)
        site = self.docs("site")
        deployment = next(
            d
            for d in site
            if d["kind"] == "Deployment"
            and d["metadata"]["name"] == "hibana-control-plane"
        )
        pod = deployment["spec"]["template"]["spec"]
        container = next(c for c in pod["containers"] if c["name"] == "control-plane")
        env = {v["name"]: v.get("value") for v in container["env"]}
        self.assertEqual(env["OIDC_CA_CERT_FILE"], "/etc/hibana-ca/ca.crt")
        ca = next(
            d
            for d in site
            if d["kind"] == "Secret" and d["metadata"]["name"] == "hibana-site-ca"
        )
        self.assertEqual(base64.b64decode(ca["data"]["ca.crt"]), b"fixture CA")


if __name__ == "__main__":
    unittest.main()
