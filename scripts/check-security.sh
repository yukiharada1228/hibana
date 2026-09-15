#!/usr/bin/env bash
# RustSec gate. Run from any directory; requires cargo-audit 0.22.2 or newer.
set -euo pipefail
cd "$(dirname "$0")/.."

"${HIBANA_CARGO_AUDIT:-cargo-audit}" audit --deny unsound "$@"
