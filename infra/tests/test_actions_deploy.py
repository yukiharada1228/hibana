import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).parents[1] / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


server = module("actions_deploy", "actions-deploy.py")
client = module("deploy_client", "deploy-client.py")
SHA = "a" * 40
TAG = "v0.2.0-rc.13"
COMMAND = "deploy " + TAG + " " + SHA


class ActionsTests(unittest.TestCase):
    def test_forced_command_rejects_shell_sftp_and_argument_injection(self):
        self.assertEqual(server.parse_command([COMMAND]), (TAG, SHA))
        for args in [[], [COMMAND, "extra"], ["sh"], ["internal-sftp"], [COMMAND + "; id"],
                     [COMMAND + "\n"], [COMMAND + " --retry"], ["deploy --help " + SHA],
                     ["deploy v1.0.0/../../tmp " + SHA], ["deploy " + TAG + " " + "b" * 39]]:
            with self.assertRaises(ValueError):
                server.parse_command(args)

    def test_client_pins_host_keys_and_rejects_options_as_destination(self):
        args = client.ssh_arguments(Path("/private"), "192.0.2.1", "2222", TAG, SHA)
        self.assertIn("StrictHostKeyChecking=yes", args)
        self.assertIn("HostKeyAlgorithms=ssh-ed25519", args)
        self.assertEqual(args[-1], COMMAND)
        for host, port, tag, sha in [("-oProxyCommand=evil", 2222, TAG, SHA),
                                     ("192.0.2.1", 22, TAG, SHA), ("192.0.2.1", 2222, "v1;id", SHA),
                                     ("192.0.2.1", 2222, TAG, "bad")]:
            with self.assertRaises(ValueError):
                client.ssh_arguments(Path("/private"), host, port, tag, sha)

    def test_remote_failure_is_not_reported_as_deployment_success(self):
        with patch("sys.argv", ["actions-deploy.py", COMMAND]), \
                patch.object(server.subprocess, "run", return_value=subprocess.CompletedProcess([], 7)):
            self.assertEqual(server.main(), 7)

    def test_success_requires_matching_committed_deployment_state(self):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path)
            with patch.object(server, "ROOT", root), patch("sys.argv", ["actions-deploy.py", COMMAND]), \
                    patch.object(server.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                for state in [{"tag": TAG, "commit": "b" * 40}, {"tag": TAG, "commit": SHA, "attempt": {"tag": TAG}}]:
                    (root / "state.json").write_text(json.dumps(state))
                    with self.assertRaises(RuntimeError):
                        server.main()
                (root / "state.json").write_text(json.dumps({"tag": TAG, "commit": SHA, "deployed_at": "2026-09-25"}))
                self.assertEqual(server.main(), 0)
                args = run.call_args.args[0]
                self.assertIn("--wait", args)
                self.assertIn("--unit=hibana-deploy", args)
                self.assertNotIn("--pipe", args)


if __name__ == "__main__":
    unittest.main()
