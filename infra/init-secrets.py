#!/usr/bin/env python3
"""Create credentials once; encrypt this file using ansible-vault before storing it."""

import argparse
import os
import pathlib
import secrets
import uuid

import yaml

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("destination", type=pathlib.Path)
a = p.parse_args()
os.umask(0o077)
keys = [
    "db_owner",
    "db_app",
    "redis",
    "s3",
    "oidc",
    "bootstrap",
    "signing",
    "master",
    "keycloak_db",
    "keycloak_admin",
    "owner_password",
]
values = {k: secrets.token_hex(32) for k in keys}
values["owner_subject"] = str(uuid.uuid4())
values["cloudflare_dns_token"] = ""
a.destination.parent.mkdir(parents=True, exist_ok=True)
with a.destination.open("x") as f:
    yaml.safe_dump({"vault_hibana": values}, f)
print(
    f"Created {a.destination}; set the DNS token and encrypt with ansible-vault. Keep a backup."
)
