"""Run the real RLS tripwire against isolated source/configuration fixtures."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("rls-lint.sh")


class RlsLintTests(unittest.TestCase):
    def check_source(self, source, *, database_user="faas_app"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("scripts", "crates/fixture/src", "migrations"):
                (root / name).mkdir(parents=True)
            shutil.copyfile(SCRIPT, root / "scripts/rls-lint.sh")
            (root / "crates/fixture/src/main.rs").write_text(source)
            (root / ".env.example").write_text(
                f"DATABASE_URL=postgres://{database_user}@localhost/fixture\n"
            )
            (root / "Makefile").write_text("")
            return subprocess.run(
                ["bash", str(root / "scripts/rls-lint.sh")],
                capture_output=True, text=True, timeout=10,
            )

    def test_current_and_future_tenant_queries_reject_the_raw_pool(self):
        for function in (
            "create_component", "find_oidc_user", "version_environment_within_limit",
            "insert_audit_log", "list_signing_keys", "future_tenant_query",
            "find_token_by_hash_extra",
        ):
            with self.subTest(function=function):
                result = self.check_source(f"crate::db::{function}(state.pool(), tenant).await?;")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("ERROR(rls-lint 2)", result.stdout)
        for function in ("resolve_for_injection", "find_token_by_hash"):
            with self.subTest(namespace="secrets", function=function):
                result = self.check_source(f"secrets::{function}( state.pool(), tenant).await?;")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("ERROR(rls-lint 2)", result.stdout)

    def test_reviewed_global_reads_and_transactions_remain_allowed(self):
        source = "\n".join(
            f"crate::db::{function}(state.pool(), value).await?;"
            for function in (
                "find_tenant_id_by_slug", "find_token_by_hash", "list_tenants_for_admin",
                "load_tenant_status_and_quotas", "tenant_is_active", "session_tenant",
                "secrets_stale_kek", "secrets_kek_kid_counts_all",
            )
        )
        source += "\ncrate::db::find_oidc_user(&tx, tenant).await?;\n"
        result = self.check_source(source)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_allowed_read_cannot_hide_an_unsafe_call_on_the_same_line(self):
        result = self.check_source(
            "db::find_token_by_hash(state.pool(), hash).await?; "
            "db::future_tenant_query(state.pool(), tenant).await?;"
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("future_tenant_query", result.stdout)
        self.assertNotIn("find_token_by_hash", result.stdout)

    def test_other_guards_remain_effective(self):
        for source, user, number in (
            ('execute("SET app.tenant_id = value");', "faas_app", 1),
            ("", "postgres", 3),
            ("let plaintext = secret.expose();", "faas_app", 4),
        ):
            with self.subTest(guard=number):
                result = self.check_source(source, database_user=user)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"ERROR(rls-lint {number})", result.stdout)


if __name__ == "__main__":
    unittest.main()
