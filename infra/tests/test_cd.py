"""Production gate/partial failure tests without a live cluster or credentials."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("cd", Path(__file__).parents[1] / "cd.py")
cd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cd)
SHA = "a" * 40
TAG = "v0.2.0-rc.13"
MARKER = {"schema": 1, "repository": cd.REPO, "tag": TAG, "commit": SHA}
RELEASE = {"draft": False, "tag_name": TAG, "published_at": "2026-09-25T12:00:00Z",
           "assets": [{"name": "production.json"}]}
STATE = {"commit": "b" * 40, "published_at": "2026-09-24T00:00:00Z", "tag": "v0.2.0-rc.12"}


class GateTests(unittest.TestCase):
    def test_only_new_published_approved_releases_are_candidates(self):
        for change in [{"draft": True}, {"assets": []}, {"published_at": STATE["published_at"]}]:
            self.assertIsNone(cd.candidate([RELEASE | change], STATE))
        self.assertEqual(cd.candidate([RELEASE], STATE), RELEASE)

    def test_marker_rejects_repository_tag_commit_and_schema_substitution(self):
        cd.validate_marker(MARKER, TAG, SHA)
        for field, value in [("repository", "attacker/hibana"), ("tag", "v1.0.0"),
                             ("commit", "c" * 40), ("schema", 2)]:
            with self.assertRaises(ValueError):
                cd.validate_marker(MARKER | {field: value}, TAG, SHA)
        for tag in ["--upload-pack=evil", "v1.0.0;id", "v1/../../tmp"]:
            with self.assertRaises(ValueError):
                cd.validate_marker(MARKER | {"tag": tag}, tag, SHA)

    def scenario(self, state=STATE, marker=MARKER, failure=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "state.json").write_text(json.dumps(state))
            (root / "source/sdk").mkdir(parents=True)
            (root / "source/sdk/package.json").write_text('{"version":"0.2.0-rc.13"}')
            (root / "site.yml").write_text("hibana:\n  version: 0.2.0-rc.12\n")
            (root / "backups").mkdir()
            def command(args, **kwargs):
                if "rev-parse" in args:
                    return SHA.encode()
                if "merge-base" in args and failure == "ancestor":
                    raise subprocess.CalledProcessError(1, args)
                return b""
            with patch.object(cd, "ROOT", root), patch("sys.argv", ["cd.py"]), \
                    patch.object(cd, "get_json", side_effect=[[RELEASE], marker]) as network, \
                    patch.object(cd, "run", side_effect=command), \
                    patch.object(cd, "backup", return_value=root / "backups/new.vault") as backup, \
                    patch.object(cd, "verify") as verify, patch.object(cd.subprocess, "run") as deploy:
                if failure == "backup":
                    backup.side_effect = RuntimeError("backup failed")
                if failure == "deploy":
                    deploy.side_effect = subprocess.CalledProcessError(1, ["ansible-playbook"])
                if failure == "verify":
                    verify.side_effect = RuntimeError("health check failed")
                error = None
                try:
                    cd.main()
                except Exception as exc:
                    error = exc
                return json.loads((root / "state.json").read_text()), error, network.call_count, deploy.call_count, verify.call_count

    def test_incomplete_attempt_stays_paused_without_network_or_mutation(self):
        state = STATE | {"attempt": {"tag": TAG}}
        result, error, network, deploy, _ = self.scenario(state=state)
        self.assertEqual(result, state)
        self.assertIsNone(error)
        self.assertEqual((network, deploy), (0, 0))

    def test_invalid_marker_or_rollback_cannot_reach_backup_and_deployment(self):
        for options in [{"marker": MARKER | {"commit": "bad"}}, {"failure": "ancestor"}]:
            state, error, _, deploy, _ = self.scenario(**options)
            self.assertIsNotNone(error)
            self.assertEqual(state, STATE)
            self.assertEqual(deploy, 0)

    def test_backup_deploy_or_health_failure_preserves_previous_release_and_pauses(self):
        for failure in ["backup", "deploy", "verify"]:
            state, error, _, deploy, _ = self.scenario(failure=failure)
            self.assertIsNotNone(error)
            self.assertEqual(state["commit"], STATE["commit"])
            self.assertEqual(state["attempt"]["tag"], TAG)
            if failure == "backup":
                self.assertEqual(deploy, 0)

    def test_success_records_release_only_after_deployment_and_verification(self):
        state, error, _, deploy, verify = self.scenario()
        self.assertIsNone(error)
        self.assertNotIn("attempt", state)
        self.assertEqual(state["commit"], SHA)
        self.assertEqual(state["tag"], TAG)
        self.assertEqual((deploy, verify), (1, 1))


if __name__ == "__main__":
    unittest.main()
