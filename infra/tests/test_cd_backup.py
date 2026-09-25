"""Exercise disk-backed backups with real tar/GnuPG and a streaming DB fixture."""

import importlib.util
import os
from types import SimpleNamespace
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import tracemalloc
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("cd", Path(__file__).parents[1] / "cd.py")
cd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cd)


class BackupTests(unittest.TestCase):
    def setUp(self):
        self.previous_umask = os.umask(0o077)
        # macOS's default TMPDIR is too long for GnuPG's Unix socket; keep the
        # fixture path comparable to the fixed /opt/hibana/cd production path.
        self.temporary = tempfile.TemporaryDirectory(prefix="hb-backup-", dir="/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.addCleanup(os.umask, self.previous_umask)
        self.root = Path(self.temporary.name) / "cd"
        self.root.mkdir()
        (self.root / "vault.key").write_text("test-only-high-entropy-backup-passphrase\n")
        (self.root / "site.yml").write_text("hibana: {version: test}\n")
        rendered = self.root.parent / "rendered"
        rendered.mkdir()
        (rendered / "secret").write_text("fixture-only-secret")
        self.kube = self.root.parent / "kubectl.py"
        self.kube.write_text('''import os, sys
if "pg_dump" in sys.argv:
    sys.stdout.buffer.write(b"PGDMP")
    for _ in range(32):
        sys.stdout.buffer.write(b"x" * 1024 * 1024)
    if os.environ.get("FAIL_BACKUP") == "dump":
        sys.exit(1)
elif "pg_restore" in sys.argv:
    assert sys.stdin.buffer.read(5) == b"PGDMP"
    size = 0
    while chunk := sys.stdin.buffer.read(65536):
        size += len(chunk)
    assert size == 32 * 1024 * 1024
    if os.environ.get("FAIL_BACKUP") == "restore":
        sys.exit(1)
else:
    print('{"items":[]}')
''')

    def backup(self):
        with patch.object(cd, "ROOT", self.root), patch.object(cd, "KUBE", [sys.executable, str(self.kube)]):
            return cd.backup("0.2.0-rc.13")

    def decrypt(self, backup, output, key=None):
        home = self.root / "gnupg"
        home.mkdir(exist_ok=True)
        try:
            return subprocess.run([
                "gpg", "--no-options", "--homedir", str(home), "--batch", "--yes",
                "--pinentry-mode", "loopback", "--no-symkey-cache", "--passphrase-file",
                str(key or self.root / "vault.key"), "--output", str(output), "--decrypt", str(backup),
            ], capture_output=True, timeout=30)
        finally:
            subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "gpg-agent"], check=True, timeout=30)

    def test_large_dumps_have_bounded_python_memory_and_restore_exact_contents(self):
        tracemalloc.start()
        try:
            backup = self.backup()
            _, peak = tracemalloc.get_traced_memory()
        finally:
            tracemalloc.stop()
        self.assertLess(peak, 12 * 1024 * 1024, f"64 MiB of DB fixtures consumed {peak} bytes")
        self.assertEqual(backup.stat().st_mode & 0o077, 0)
        self.assertEqual(list(backup.parent.glob(".backup-*")), [])
        plain = self.root / "restored.tar.gz"
        result = self.decrypt(backup, plain)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        with tarfile.open(plain) as archive:
            self.assertEqual(set(archive.getnames()), {
                "hibana.json", "hibana-identity.json", "hibana-edge.json", "rendered.tar.gz",
                "site.yml", "hibana.dump", "keycloak.dump",
            })
            for name in ["hibana.dump", "keycloak.dump"]:
                self.assertEqual(archive.getmember(name).size, 32 * 1024 * 1024 + 5)
                with archive.extractfile(name) as source:
                    self.assertEqual(source.read(5), b"PGDMP")
                    while chunk := source.read(65536):
                        self.assertEqual(chunk, b"x" * len(chunk))
        wrong_key = self.root / "wrong.key"
        wrong_key.write_text("wrong-backup-key\n")
        self.assertNotEqual(self.decrypt(backup, plain, wrong_key).returncode, 0)
        damaged = self.root / "damaged.gpg"
        data = bytearray(backup.read_bytes())
        data[-10] ^= 1
        damaged.write_bytes(data)
        self.assertNotEqual(self.decrypt(damaged, plain).returncode, 0)

    def test_partial_dumps_or_invalid_restores_never_publish_a_backup(self):
        stale = self.root / "backups" / ".backup-killed-process"
        stale.mkdir(parents=True)
        (stale / "plaintext.dump").write_text("interrupted fixture backup")
        for failure in ["dump", "restore"]:
            with patch.dict(os.environ, {"FAIL_BACKUP": failure}):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.backup()
            self.assertEqual(list((self.root / "backups").iterdir()), [])

    def test_disk_headroom_refuses_backup_and_caps_subprocess_output(self):
        # Refuse both before dumping and before archive creation (after dumps).
        for free in [cd.DISK_RESERVE, cd.DISK_RESERVE + 100 * 1024**2]:
            with patch.object(cd.shutil, "disk_usage", return_value=SimpleNamespace(free=free)):
                with self.assertRaises(OSError):
                    self.backup()
            self.assertEqual(list((self.root / "backups").iterdir()), [])
        output = self.root / "limited.dump"
        with patch.object(cd, "ROOT", self.root), \
                patch.object(cd.shutil, "disk_usage", return_value=SimpleNamespace(free=cd.DISK_RESERVE + 1024)):
            with self.assertRaises(subprocess.CalledProcessError):
                cd.capture_file([sys.executable, str(self.kube), "pg_dump"], output)
        self.assertLessEqual(output.stat().st_size, 1024)

    def test_failed_encryption_or_verification_keeps_existing_backups(self):
        backups = self.root / "backups"
        backups.mkdir()
        previous = backups / "previous.vault"
        previous.write_bytes(b"previous encrypted backup")
        run = subprocess.run
        for failure in ["--symmetric", "--decrypt"]:
            def failing_run(command, **kwargs):
                if command[0] == "gpg" and failure in command:
                    raise subprocess.CalledProcessError(1, command)
                return run(command, **kwargs)
            with patch.object(cd.subprocess, "run", side_effect=failing_run):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.backup()
            self.assertEqual(list(backups.iterdir()), [previous])
            self.assertEqual(previous.read_bytes(), b"previous encrypted backup")


if __name__ == "__main__":
    unittest.main()
