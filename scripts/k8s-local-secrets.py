#!/usr/bin/env python3
"""Generate development credentials once, without printing them or rotating existing data."""
import json
import os
from pathlib import Path
import secrets
import shlex
import sys

state = Path(sys.argv[1])
state.mkdir(parents=True, exist_ok=True)
os.chmod(state, 0o700)
target = state / "secrets.json"
if target.exists():
    # Remove only credentials for the retired messaging service; keep all live keys.
    saved = json.loads(target.read_text())
    for item in saved["items"]:
        for key in list(item.get("stringData", {})):
            if key.startswith("NATS_"):
                del item["stringData"][key]
    target.write_text(json.dumps(saved))
    os.chmod(target, 0o600)
    sys.exit(0)

admin, app, redis, s3, bootstrap, login = [secrets.token_hex(24) for _ in range(6)]
items = []
def secret(name, data):
    items.append({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name, "namespace": "hibana"}, "type": "Opaque", "stringData": data})

secret("hibana-runtime", {
    "DATABASE_URL": f"postgres://faas_app:{app}@hibana-postgres:5432/hibana",
})
secret("hibana-control-plane", {
    "REDIS_URL": f"redis://:{redis}@hibana-redis:6379",
    "S3_ACCESS_KEY": "hibana-local",
    "S3_SECRET_KEY": s3,
    "BOOTSTRAP_ADMIN_TOKEN": bootstrap,
    "JOB_SIGNING_KEY": secrets.token_hex(32),
    "SECRETS_MASTER_KEY": secrets.token_hex(32),
})
secret("hibana-migration", {"MIGRATION_DATABASE_URL": f"postgres://hibana_admin:{admin}@hibana-postgres:5432/hibana"})
secret("hibana-local-dependencies", {
    "POSTGRES_PASSWORD": admin, "POSTGRES_APP_PASSWORD": app,
    "REDIS_PASSWORD": redis, "MINIO_ROOT_USER": "hibana-local", "MINIO_ROOT_PASSWORD": s3,
})
os.umask(0o077)
target.write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": items}))
(state / "sdk.env").write_text("".join(f"{k}={shlex.quote(v)}\n" for k, v in {
    "HIBANA_URL": "http://127.0.0.1:18080", "HIBANA_TENANT": "smoke",
    "HIBANA_EMAIL": "admin@example.com", "HIBANA_PASSWORD": login,
    "HIBANA_INGRESS_DOMAIN": "hibana.local", "BOOTSTRAP_ADMIN_TOKEN": bootstrap,
}.items()))
