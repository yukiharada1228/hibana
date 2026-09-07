#!/usr/bin/env bash
# RustSec gate. Run from any directory; requires cargo-audit 0.22.2 or newer.
set -euo pipefail
cd "$(dirname "$0")/.."

# SQLx 0.8 records its optional MySQL/RSA dependency in Cargo.lock even though
# Hibana only enables PostgreSQL. Never suppress the RSA advisory if that crate
# becomes part of any target's actual build (including tests and build scripts).
rsa_tree=$(cargo tree --locked --workspace --target all --invert rsa --prefix none)
if [[ -n "$rsa_tree" ]]; then
  echo "Security gate: rsa is now compiled; RUSTSEC-2023-0071 must be resolved." >&2
  exit 1
fi

"${HIBANA_CARGO_AUDIT:-cargo-audit}" audit --deny unsound --ignore RUSTSEC-2023-0071 "$@"
