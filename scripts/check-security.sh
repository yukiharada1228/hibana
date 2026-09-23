#!/usr/bin/env bash
# RustSec gate. Run from any directory; requires cargo-audit 0.22.2 or newer.
set -euo pipefail
cd "$(dirname "$0")/.."

# This exception applies only to public-key signature verification through the
# reviewed OIDC dependency. Fail before audit if its graph, APIs or review date
# change; unrelated advisories and unsound warnings still fail the audit.
rsa_tree=$(cargo tree --locked --workspace --target all --invert rsa --prefix none --no-dedupe)
printf '%s\n' "$rsa_tree" | python3 scripts/check_oidc_rsa.py
"${HIBANA_CARGO_AUDIT:-cargo-audit}" audit --deny unsound --ignore RUSTSEC-2023-0071 "$@"
