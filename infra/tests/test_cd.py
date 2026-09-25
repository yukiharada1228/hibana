"""Production gate/partial failure tests without a live cluster or credentials."""

import importlib.util
import fcntl
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
    def test_unpublished_or_wrong_release_cannot_deploy(self):
        for change in [{"draft": True}, {"tag_name": "v1.0.0"}, {"published_at": None}]:
            state, error, _, deploy, _ = self.scenario(release=RELEASE | change)
            self.assertIsNotNone(error)
            self.assertEqual(state, STATE)
            self.assertEqual(deploy, 0)

    def test_marker_rejects_repository_tag_commit_and_schema_substitution(self):
        cd.validate_marker(MARKER, TAG, SHA)
        for field, value in [("repository", "attacker/hibana"), ("tag", "v1.0.0"),
                             ("commit", "c" * 40), ("schema", 2)]:
            with self.assertRaises(ValueError):
                cd.validate_marker(MARKER | {field: value}, TAG, SHA)
        for tag in ["--upload-pack=evil", "v1.0.0;id", "v1/../../tmp"]:
            with self.assertRaises(ValueError):
                cd.validate_marker(MARKER | {"tag": tag}, tag, SHA)

    def scenario(self, state=STATE, marker=MARKER, failure=None, release=RELEASE, retry=False, maintenance=False,
                 locked=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "state.json").write_text(json.dumps(state))
            (root / "source/sdk").mkdir(parents=True)
            (root / "source/sdk/package.json").write_text('{"version":"0.2.0-rc.13"}')
            (root / "site.yml").write_text("hibana:\n  version: 0.2.0-rc.12\n")
            (root / "backups").mkdir()
            if maintenance:
                (root / "maintenance").touch()
            lock = (root / "lock").open("w")
            if locked:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            def command(args, **kwargs):
                if "rev-parse" in args:
                    return SHA.encode()
                if "merge-base" in args and failure == "ancestor":
                    raise subprocess.CalledProcessError(1, args)
                if "merge-base" in args and failure == "attempt_ancestor" and args[-2] == "c" * 40:
                    raise subprocess.CalledProcessError(1, args)
                return b""
            argv = ["cd.py", "--tag", TAG, "--commit", SHA] + (["--retry"] if retry else [])
            with patch.object(cd, "ROOT", root), patch("sys.argv", argv), \
                    patch.object(cd, "get_json", side_effect=[release, marker]) as network, \
                    patch.object(cd, "run", side_effect=command), \
                    patch.object(cd, "backup", return_value=root / "backups/new.tar.gz.gpg") as backup, \
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
                finally:
                    lock.close()
                return json.loads((root / "state.json").read_text()), error, network.call_count, deploy.call_count, verify.call_count

    def test_incomplete_attempt_stays_paused_without_network_or_mutation(self):
        state = STATE | {"attempt": {"tag": TAG}}
        result, error, network, deploy, _ = self.scenario(state=state)
        self.assertEqual(result, state)
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual((network, deploy), (0, 0))

    def test_maintenance_refuses_even_operator_retry_without_any_network_or_deployment(self):
        for retry in [False, True]:
            state, error, network, deploy, _ = self.scenario(maintenance=True, retry=retry)
            self.assertEqual(state, STATE)
            self.assertIn("maintenance", str(error))
            self.assertEqual((network, deploy), (0, 0))

    def test_actual_lock_contention_refuses_deployment_before_any_mutation(self):
        state, error, network, deploy, _ = self.scenario(locked=True)
        self.assertEqual(state, STATE)
        self.assertIsInstance(error, BlockingIOError)
        self.assertEqual((network, deploy), (0, 0))

    def test_retry_cannot_deploy_behind_a_partially_migrated_release(self):
        state = STATE | {"attempt": {"tag": "v0.2.0-rc.14", "commit": "c" * 40}}
        result, error, _, deploy, _ = self.scenario(state=state, retry=True, failure="attempt_ancestor")
        self.assertEqual(result, state)
        self.assertIsInstance(error, subprocess.CalledProcessError)
        self.assertEqual(deploy, 0)
        # An operator can retry that commit or a reviewed fix that descends from it.
        result, error, _, deploy, _ = self.scenario(state=state, retry=True)
        self.assertIsNone(error)
        self.assertNotIn("attempt", result)
        self.assertEqual(deploy, 1)

    def test_failed_retry_keeps_the_original_pre_migration_backup(self):
        state = STATE | {"attempt": {"tag": TAG, "commit": SHA, "backup": "/private/original.vault"}}
        for failure in ["backup", "deploy", "verify"]:
            result, error, _, _, _ = self.scenario(state=state, retry=True, failure=failure)
            self.assertIsNotNone(error)
            self.assertEqual(result["attempt"]["backup"], "/private/original.vault")

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
                self.assertNotIn("backup", state["attempt"])
            else:
                self.assertTrue(state["attempt"]["backup"].endswith("new.tar.gz.gpg"))

    def test_success_records_release_only_after_deployment_and_verification(self):
        state, error, _, deploy, verify = self.scenario()
        self.assertIsNone(error)
        self.assertNotIn("attempt", state)
        self.assertEqual(state["commit"], SHA)
        self.assertEqual(state["tag"], TAG)
        self.assertEqual((deploy, verify), (1, 1))

    def test_retention_handles_both_formats_and_does_not_fail_a_verified_deploy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "backups").mkdir()
            paths = [root / "backups" / name for name in [
                "20260901.vault", "20260902.vault", "20260903.tar.gz.gpg", "20260904.tar.gz.gpg"]]
            for path in paths:
                path.touch()
            with patch.object(cd, "ROOT", root), patch.object(Path, "unlink", side_effect=PermissionError):
                cd.prune_backups()  # Cleanup is best effort; do not report a failed rollout.
            self.assertTrue(all(path.exists() for path in paths))
            with patch.object(cd, "ROOT", root):
                cd.prune_backups()
            self.assertFalse(paths[0].exists())
            self.assertTrue(all(path.exists() for path in paths[1:]))


if __name__ == "__main__":
    unittest.main()
