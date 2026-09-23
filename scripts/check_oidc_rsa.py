#!/usr/bin/env python3
"""Fail closed if the reviewed, verification-only RSA use changes.

This is a dependency/API tripwire, not a Rust parser or a security proof.
See docs/security.md for the assessment and its upstream evidence.
"""
from datetime import date, datetime, timezone
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
REVIEW_BY = date(2026, 12, 20)
OIDC_FILES = {
    "crates/control-plane/src/oidc/mod.rs",
    "crates/control-plane/src/oidc/config.rs",
    "crates/control-plane/src/oidc/http.rs",
}
# These are the only openidconnect APIs currently used by Hibana. New APIs,
# aliases and glob imports require an explicit reassessment of the exception.
OIDC_API = set("""
core CoreAuthenticationFlow CoreClient CoreClientAuthMethod CoreProviderMetadata
AccessTokenHash AuthType AuthorizationCode ClientId ClientSecret CsrfToken IssuerUrl
Nonce OAuth2TokenResponse PkceCodeChallenge PkceCodeVerifier RedirectUrl TokenResponse
EndpointSet EndpointNotSet EndpointMaybeSet AsyncHttpClient HttpRequest HttpResponse
http Response builder
Scope new
""".split())


def validate(tree, sources, today, root=ROOT):
    if today >= REVIEW_BY:
        raise ValueError(f"RSA applicability assessment expired on {REVIEW_BY}; review it again")
    lines = [line.strip() for line in tree.splitlines() if line.strip()]
    if (len(lines) != 3 or lines[:2] != ["rsa v0.9.10", "openidconnect v4.0.1"]
            or not re.fullmatch(
                rf"hibana-control-plane v[^ ]+ \({re.escape(str(root / 'crates/control-plane'))}\)",
                lines[2])):
        raise ValueError("RSA dependency/version changed; reassess RUSTSEC-2023-0071")
    for name, source in sources.items():
        if re.search(r"\brsa\s*::|\b(?:RsaPrivateKey|CoreRsaPrivateSigningKey|PrivateSigningKey)\b", source):
            raise ValueError(f"{name}: RSA/private signing API is outside the assessed use")
        if not re.search(r"\bopenidconnect\b", source):
            continue
        if name not in OIDC_FILES or re.search(r"\bextern\s+crate\b", source):
            raise ValueError(f"{name}: new OIDC consumer requires review")
        imports = []
        for expression in re.findall(r"\buse\s+([^;]+);", source, re.S):
            if re.search(r"\bopenidconnect\b", expression):
                if not re.match(r"openidconnect\s*::", expression):
                    raise ValueError(f"{name}: OIDC alias/grouped import requires review")
                imports.append(re.sub(r"^openidconnect\s*::\s*", "", expression))
        paths = re.findall(r"\bopenidconnect\s*::\s*((?:\w+\s*::\s*)*\w+)", source)
        for expression in imports + paths:
            if "*" in expression or set(re.findall(r"\b\w+\b", expression)) - OIDC_API:
                raise ValueError(f"{name}: new OIDC API requires review")


def main():
    sources = {
        str(path.relative_to(ROOT)): path.read_text()
        for directory in ("crates", "migrations")
        for path in (ROOT / directory).rglob("*.rs")
    }
    validate(sys.stdin.read(), sources, datetime.now(timezone.utc).date())
    print("RUSTSEC-2023-0071: reviewed as not applicable to OIDC public-key verification; "
          f"rsa 0.9.10 via openidconnect 4.0.1 only; reassess before {REVIEW_BY}")


if __name__ == "__main__":
    try:
        main()
    except ValueError as error:
        raise SystemExit(f"Security gate: {error}") from None
