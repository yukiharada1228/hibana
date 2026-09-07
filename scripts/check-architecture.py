#!/usr/bin/env python3
"""Small import/path tripwire for the boundaries described in docs/architecture.md.

This is not a Rust parser or a security proof. Compilation and integration tests
remain required; the check catches accidental dependencies on outer layers.
"""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BOUNDARIES = {
    "crates/worker/src/runtime": {
        "artifacts", "config", "control_plane", "dev", "direct_http", "lifecycle",
        "repository", "service", "axum", "sqlx", "redis",
    },
    "crates/control-plane/src/db": {
        "bootstrap", "completion", "direct_http", "handlers", "handlers_secrets",
        "ingress", "routes", "axum",
    },
}


def violations(source, forbidden):
    source = re.sub(r"/\*.*?\*/|//[^\n]*", "", source, flags=re.S)
    imports = " ".join(re.findall(r"\buse\s+([^;]+);", source, flags=re.S))
    for name in sorted(forbidden):
        if re.search(rf"\b{name}\b", imports) or re.search(rf"\b{name}\s*::", source):
            yield name


def main():
    errors = []
    for directory, forbidden in BOUNDARIES.items():
        for source in sorted((ROOT / directory).rglob("*.rs")):
            for dependency in violations(source.read_text(), forbidden):
                errors.append(f"{source.relative_to(ROOT)}: forbidden dependency {dependency}")
    if errors:
        raise SystemExit("\n".join(errors))
    print("architecture: OK (runtime independent of fleet adapters; DB independent of HTTP)")


if __name__ == "__main__":
    main()
