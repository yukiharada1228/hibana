#!/usr/bin/env python3
"""Private CA and fresh credentials for disposable .test VMs; never modifies host trust/DNS."""

import datetime as dt
import subprocess
from pathlib import Path

import yaml
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

root = Path(__file__).resolve().parents[2]
state = root / ".local/iac"
secret_path = state / "vault.yml"
if not secret_path.exists():
    subprocess.run(
        [
            str(root / ".local/iac-venv/bin/python"),
            str(root / "infra/init-secrets.py"),
            str(secret_path),
        ],
        check=True,
    )
s = yaml.safe_load(secret_path.read_text())
if "tls_crt" not in s["vault_hibana"]:
    now = dt.datetime.now(dt.timezone.utc)
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name(
        [x509.NameAttribute(NameOID.COMMON_NAME, "Hibana disposable IaC test CA")]
    )
    ca = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - dt.timedelta(minutes=5))
        .not_valid_after(now + dt.timedelta(days=7))
        .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
        .sign(key, hashes.SHA256())
    )
    server_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    server = (
        x509.CertificateBuilder()
        .subject_name(
            x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "hibana.iac.test")])
        )
        .issuer_name(name)
        .public_key(server_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - dt.timedelta(minutes=5))
        .not_valid_after(now + dt.timedelta(days=7))
        .add_extension(
            x509.SubjectAlternativeName(
                [
                    x509.DNSName(d)
                    for d in [
                        "hibana.iac.test",
                        "auth.hibana.iac.test",
                        "*.local.apps.hibana.iac.test",
                    ]
                ]
            ),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )
    s["vault_hibana"].update(
        tls_crt=server.public_bytes(serialization.Encoding.PEM).decode(),
        tls_key=server_key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        ).decode(),
        ca_crt=ca.public_bytes(serialization.Encoding.PEM).decode(),
    )
    secret_path.write_text(yaml.safe_dump(s))
    secret_path.chmod(0o600)
(state / "ca.crt").write_text(s["vault_hibana"]["ca_crt"])
c = yaml.safe_load((root / "infra/site.example.yml").read_text())
c["hibana"].update(
    domain="hibana.iac.test",
    admin_email="owner@hibana.iac.test",
    management_node="hibana-iac-management",
    execution_node="hibana-iac-execution",
    architecture="arm64",
    tls_mode="provided",
)
(state / "site.yml").write_text(yaml.safe_dump(c))
# The test names resolve only on these dedicated guests. Public DNS and Mac hosts/trust stay untouched.
inv = yaml.safe_load((state / "inventory.yml").read_text())
management = inv["all"]["children"]["management"]["hosts"]["hibana-iac-management"][
    "node_ip"
]
line = f"{management} hibana.iac.test auth.hibana.iac.test hello.local.apps.hibana.iac.test"
for name in ["hibana-iac-cp", "hibana-iac-management", "hibana-iac-execution"]:
    subprocess.run(
        [
            "limactl",
            "shell",
            name,
            "sudo",
            "python3",
            "-c",
            "from pathlib import Path; p=Path('/etc/hosts'); lines=[l for l in p.read_text().splitlines() if 'hibana.iac.test' not in l]; p.write_text('\\n'.join(lines+["
            + repr(line)
            + "])+ '\\n')",
        ],
        check=True,
    )
print("Prepared isolated test configuration; no host DNS or CA trust changes.")
