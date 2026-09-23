"""Offline contract between platform init, the install fixture and preflight."""
from copy import deepcopy
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch

import yaml

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("platform_install_fixture", ROOT / "scripts/test-platform-install.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from common import KubernetesTarget, environment_values, secret_values
from preflight import Preflight, oidc_setting_errors


class InstallFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        subprocess.run(["node", "sdk/scripts/pack.mjs"], cwd=ROOT, check=True, capture_output=True)

    def test_generated_site_passes_preflight_and_preserves_policies_during_cutover(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            cluster = fixture.LocalCluster("hibana-fixture")
            cluster.state = folder / "state"
            with patch.dict(os.environ, {"OIDC_CLIENT_SECRET": "operator-secret", "AUTH_MODE": "password"}):
                credentials = fixture.fixture_credentials(cluster)
                self.assertEqual(os.environ["OIDC_CLIENT_SECRET"], "operator-secret")
                self.assertEqual(os.environ["AUTH_MODE"], "password")
            control = next(d["stringData"] for d in credentials if d["metadata"]["name"] == "hibana-control-plane")
            self.assertEqual(control["OIDC_CLIENT_SECRET"], "fixture-only")
            self.assertNotIn("AUTH_MODE", control)
            site = folder / "site"
            subprocess.run(["node", "--input-type=module", "--eval",
                            "import { initPlatform } from './sdk/src/platform-init.mjs'; await initPlatform(process.argv[1]);",
                            str(site)], cwd=ROOT, check=True, capture_output=True)
            policies, dependencies = fixture.configure_fixture_site(site, credentials, "hibana-dependencies", "10.96.1.10")
            identity_policy = deepcopy(next(p for p in policies if p["metadata"]["name"] == "hibana-site-identity-provider"))
            self.assertEqual(identity_policy["spec"]["egress"], [])
            # The live suite fills these with its additional rollout regressions.
            fixture.write_yaml(site / "regressions.yaml", {
                "apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "fixture-extra"}, "data": {"check": "ok"}})
            fixture.write_yaml(site / "runtime-check.yaml", {
                "apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "hibana-worker"}, "spec": {"replicas": 1}})
            renderer = KubernetesTarget()
            for address in ("10.96.1.10", "10.96.1.11"):
                with self.subTest(database=address):
                    dependencies["spec"]["egress"][-1]["to"][0]["ipBlock"]["cidr"] = address + "/32"
                    fixture.save_fixture_egress(site, policies)
                    saved = list(yaml.safe_load_all((site / "egress.yaml").read_text()))
                    self.assertEqual(len(saved), 2)
                    self.assertIn(identity_policy, saved)
                    docs = renderer.render(site, "registry.test/hibana:fixture")
                    migration = renderer.render(site / "migration", "registry.test/hibana:fixture")
                    target = MagicMock()
                    target.get.return_value = None
                    resolved = Preflight(target).settings(docs + migration)
                    target.get.assert_not_called()
                    available = {(d["kind"], d["metadata"]["name"]): d for d in resolved}
                    self.assertFalse(any(d["kind"] == "Ingress" for d in docs))
                    self.assertNotIn(("Deployment", "hibana-console"), available)
                    cp = available[("Deployment", "hibana-control-plane")]["spec"]["template"]["spec"]["containers"][0]

                    def values(kind, name, key=None, optional=False, *, text=False):
                        doc = available[(kind, name)]
                        return secret_values(doc, key) if kind == "Secret" else doc.get("data", {})

                    env = environment_values(cp, values)
                    self.assertEqual(oidc_setting_errors(env), [])
                    self.assertEqual(env["OIDC_CLIENT_SECRET"], "fixture-only")
                    self.assertEqual(env["OIDC_ISSUER_URL"], "https://fixture-idp.invalid")
                    for doc in docs:
                        if doc["kind"] == "ConfigMap":
                            self.assertNotIn("OIDC_CLIENT_SECRET", doc.get("data", {}))


if __name__ == "__main__":
    unittest.main()
