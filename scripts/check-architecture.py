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
        "repository", "service", "axum", "sqlx", "sea_orm", "hibana_database", "redis",
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
    # Fixtures may use SQL to seed invalid states; serving code must use the ORM.
    raw_sql = re.compile(r"\bsqlx\s*::|\bStatement\s*::|\.(?:execute_raw|execute_unprepared|query_one_raw|query_all_raw)\s*\(")
    for directory in ["crates/control-plane/src", "crates/worker/src", "crates/database/src"]:
        for source in sorted((ROOT / directory).rglob("*.rs")):
            if source.name in {"tests.rs", "http_tests.rs"}:
                continue
            code = source.read_text().split("#[cfg(test)]\nmod tests", 1)[0]
            code = re.sub(r"/\*.*?\*/|//[^\n]*", "", code, flags=re.S)
            if raw_sql.search(code):
                errors.append(f"{source.relative_to(ROOT)}: raw SQL outside migrations/test fixtures")
    if errors:
        raise SystemExit("\n".join(errors))
    print("architecture: OK (runtime independent of fleet adapters; DB independent of HTTP)")


if __name__ == "__main__":
    main()
