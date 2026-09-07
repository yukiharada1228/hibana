#!/usr/bin/env bash
# Bootstrap the isolated kind tenant, then run the Hibana CLI and core Wasm suite.
set -euo pipefail
cd "$(dirname "$0")/.."
test -f .local/kubernetes/sdk.env || { echo 'Run scripts/k8s-local.sh up first.' >&2; exit 1; }
set -a
. .local/kubernetes/sdk.env
set +a
python3 - <<'PY'
import json
import os
import urllib.error
import urllib.request

login = urllib.request.Request(os.environ["HIBANA_URL"] + "/auth/login", data=json.dumps({
    "tenant_slug": os.environ["HIBANA_TENANT"],
    "email": os.environ["HIBANA_EMAIL"],
    "password": os.environ["HIBANA_PASSWORD"],
}).encode(), headers={"Content-Type": "application/json"})
try:
    with urllib.request.urlopen(login, timeout=15) as response:
        assert response.status in (200, 201), response.status
        print("Smoke tenant credentials verified.")
        raise SystemExit(0)
except urllib.error.HTTPError as error:
    if error.code != 401:
        raise SystemExit(f"Smoke login failed: HTTP {error.code}") from None

body = json.dumps({
    "slug": os.environ["HIBANA_TENANT"],
    "name": "Kubernetes Smoke Tenant",
    "admin_email": os.environ["HIBANA_EMAIL"],
    "admin_password": os.environ["HIBANA_PASSWORD"],
}).encode()
request = urllib.request.Request(os.environ["HIBANA_URL"] + "/admin/tenants", data=body, headers={
    "Authorization": "Bearer " + os.environ["BOOTSTRAP_ADMIN_TOKEN"],
    "Content-Type": "application/json",
})
try:
    with urllib.request.urlopen(request, timeout=15) as response:
        assert response.status == 201, response.status
        print("Smoke tenant created.")
except urllib.error.HTTPError as error:
    if error.code != 409:
        raise SystemExit(f"Bootstrap failed: HTTP {error.code}") from None
    print("Smoke tenant already exists; the smoke test will verify its credentials.")
PY
GATEWAY="${APP_GATEWAY:-http://localhost:18084}" TENANT="$HIBANA_TENANT" bash scripts/smoke.sh
