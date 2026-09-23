"""The RSA assessment must not silently cover new consumers or private keys."""
from datetime import date
import unittest

from check_oidc_rsa import ROOT, REVIEW_BY, validate


class RsaAssessmentTests(unittest.TestCase):
    def setUp(self):
        self.tree = ("rsa v0.9.10\nopenidconnect v4.0.1\n"
                     f"hibana-control-plane v0.2.0-rc.2 ({ROOT}/crates/control-plane)\n")
        self.sources = {"crates/control-plane/src/oidc/mod.rs":
                        "use openidconnect::{core::CoreClient, TokenResponse};"}
        self.today = date(2026, 9, 21)

    def test_reviewed_public_verification_and_release_bump(self):
        validate(self.tree, self.sources, self.today)
        validate(self.tree.replace("0.2.0-rc.2", "0.2.0-rc.3"), self.sources, self.today)

    def test_reviewed_email_scope_and_current_consumers(self):
        self.sources["crates/control-plane/src/oidc/mod.rs"] += (
            '\nlet scope = openidconnect::Scope::new("email".into());'
        )
        validate(self.tree, self.sources, self.today)
        sources = {
            str(path.relative_to(ROOT)): path.read_text()
            for directory in ("crates", "migrations")
            for path in (ROOT / directory).rglob("*.rs")
        }
        validate(self.tree, sources, self.today)

    def test_new_versions_consumers_and_patched_crates_require_review(self):
        for tree in (
            "", self.tree + "another-consumer v1.0.0\n",
            self.tree.replace("rsa v0.9.10", "rsa v0.10.0"),
            self.tree.replace("openidconnect v4.0.1", "openidconnect v4.0.2"),
            self.tree.replace("rsa v0.9.10", "rsa v0.9.10 (/tmp/fork)"),
            self.tree.replace("openidconnect v4.0.1", "openidconnect v4.0.1 (/tmp/fork)"),
        ):
            with self.subTest(tree=tree), self.assertRaises(ValueError):
                validate(tree, self.sources, self.today)

    def test_new_private_key_apis_aliases_and_globs_require_review(self):
        for code in (
            "use openidconnect::core::CoreRsaPrivateSigningKey as Key;",
            "use openidconnect::{PrivateSigningKey};",
            "use openidconnect::{core::*};",
            "use openidconnect as oidc;",
            "use {openidconnect as oidc};",
            "use openidconnect::{core::CoreClient as Client};",
            "let key = openidconnect::core::CoreRsaPrivateSigningKey::from_pem(pem);",
            "let key = rsa::RsaPrivateKey::new(rng, 2048);",
            'let key = openidconnect::UnknownApi::new("value");',
        ):
            with self.subTest(code=code), self.assertRaises(ValueError):
                validate(self.tree, {"crates/worker/src/extra.rs": code}, self.today)
            with self.subTest(inside_oidc=code), self.assertRaises(ValueError):
                validate(self.tree, {"crates/control-plane/src/oidc/mod.rs": code}, self.today)

    def test_assessment_expires(self):
        with self.assertRaisesRegex(ValueError, "expired"):
            validate(self.tree, self.sources, REVIEW_BY)


if __name__ == "__main__":
    unittest.main()
