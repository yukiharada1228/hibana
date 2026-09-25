#!/usr/bin/env python3
"""Deploy an explicitly requested, CI-approved release on the control plane."""

import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

REPO = "yukiharada1228/hibana"
ROOT = Path("/opt/hibana/cd")
KUBE = ["kubectl", "--kubeconfig", "/etc/kubernetes/admin.conf"]


def run(args, **kwargs):
    return subprocess.check_output(args, timeout=1800, **kwargs)


def get_json(url):
    request = urllib.request.Request(url, headers={"User-Agent": "hibana-production-cd"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def validate_marker(marker, tag, sha):
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?", tag):
        raise ValueError("Invalid release tag")
    if not re.fullmatch(r"[a-f0-9]{40}", sha):
        raise ValueError("Invalid commit")
    if marker != {"schema": 1, "repository": REPO, "tag": tag, "commit": sha}:
        raise ValueError("Release marker does not match its immutable source")


def write_state(state):
    temporary = ROOT / "state.tmp"
    temporary.write_text(json.dumps(state, indent=2) + "\n")
    temporary.replace(ROOT / "state.json")


def backup(version):
    # DB growth must consume disk, not the deployment service's 512 MiB memory.
    # Publish only after a full decrypt/hash check, retaining old backups on error.
    directory = ROOT / "backups"
    directory.mkdir(mode=0o700, exist_ok=True)
    # main holds the deployment lock. A killed process may have left private
    # plaintext staging files; discard only its unpublished temporary workspace.
    for stale in directory.glob(".backup-*"):
        shutil.rmtree(stale)
    with tempfile.TemporaryDirectory(prefix=".backup-", dir=directory) as temporary:
        work = Path(temporary)
        files = work / "files"
        files.mkdir(mode=0o700)
        for namespace in ["hibana", "hibana-identity", "hibana-edge"]:
            capture_file(KUBE + ["-n", namespace, "get",
                "deployments,configmaps,secrets,services,ingresses,networkpolicies,persistentvolumeclaims",
                "-o", "json"], files / (namespace + ".json"))
        capture_file(["tar", "-C", str(ROOT.parent), "-czf", "-", "rendered"], files / "rendered.tar.gz")
        shutil.copyfile(ROOT / "site.yml", files / "site.yml")
        for database, namespace, deployment, user in [
            ("hibana", "hibana", "hibana-postgres", "hibana_admin"),
            ("keycloak", "hibana-identity", "keycloak-postgres", "keycloak"),
        ]:
            dump = files / (database + ".dump")
            capture_file(KUBE + ["-n", namespace, "exec", "deployment/" + deployment,
                                "--", "pg_dump", "-U", user, "-d", database, "-Fc"], dump)
            with dump.open("rb", buffering=0) as source:
                if source.read(5) != b"PGDMP":
                    raise RuntimeError("Invalid database backup")
                source.seek(0)
                subprocess.run(KUBE + ["-n", namespace, "exec", "-i", "deployment/" + deployment,
                                      "--", "pg_restore", "--list"], stdin=source,
                               stdout=subprocess.DEVNULL, check=True, timeout=1800)
        archive = work / "backup.tar.gz"
        with tarfile.open(archive, mode="w:gz") as bundle:
            for path in sorted(files.iterdir()):
                bundle.add(path, arcname=path.name)
        # An isolated keyring and no passphrase caching keep the existing Vault
        # key off argv and out of the operator's normal GnuPG configuration.
        home = work / "gnupg"
        home.mkdir(mode=0o700)
        gpg = ["gpg", "--no-options", "--homedir", str(home), "--batch", "--yes",
               "--pinentry-mode", "loopback", "--no-symkey-cache", "--passphrase-file", str(ROOT / "vault.key")]
        encrypted, decrypted = work / "backup.gpg", work / "verified.tar.gz"
        try:
            subprocess.run(gpg + ["--rfc4880", "--cipher-algo", "AES256", "--compress-algo", "none",
                                  "--output", str(encrypted), "--symmetric", str(archive)], check=True, timeout=1800)
            subprocess.run(gpg + ["--output", str(decrypted), "--decrypt", str(encrypted)], check=True, timeout=1800)
            if file_hash(archive) != file_hash(decrypted):
                raise RuntimeError("Backup encryption verification failed")
        finally:
            subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "gpg-agent"],
                           check=False, timeout=30)
        target = directory / (datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
                              + "-" + version + ".tar.gz.gpg")
        encrypted.replace(target)
    return target


def capture_file(command, destination):
    with destination.open("wb") as output:
        subprocess.run(command, stdout=output, check=True, timeout=1800)


def file_hash(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").digest()


def prune_backups():
    directory = ROOT / "backups"
    backups = sorted([*directory.glob("*.vault"), *directory.glob("*.tar.gz.gpg")])
    for old in backups[:-3]:
        try:
            old.unlink()
        except OSError as error:
            # Deployment already succeeded. Retention failure must not turn a
            # healthy release into a failed deployment or require a redeploy.
            print("Warning: could not prune backup: " + str(error), flush=True)


def verify(version, config):
    suffix = ":" + version + "-linux-" + config["architecture"]
    for name in ["hibana-control-plane", "hibana-worker", "hibana-console"]:
        run(KUBE + ["-n", "hibana", "rollout", "status", "deployment/" + name, "--timeout=600s"])
        deployment = json.loads(run(KUBE + ["-n", "hibana", "get", "deployment", name, "-o", "json"]))
        if not deployment["spec"]["template"]["spec"]["containers"][0]["image"].endswith(suffix):
            raise RuntimeError("Unexpected deployed image: " + name)
    nodes = json.loads(run(KUBE + ["get", "nodes", "-o", "json"]))["items"]
    if len(nodes) != 3 or not all(any(c["type"] == "Ready" and c["status"] == "True"
                                    for c in n["status"]["conditions"]) for n in nodes):
        raise RuntimeError("Cluster is not ready")
    domain = config["domain"]
    for url in ["https://" + domain + "/", "https://" + domain + "/api/readyz",
                "https://auth." + domain + "/realms/hibana/.well-known/openid-configuration"] + config["cd_smoke_urls"]:
        with urllib.request.urlopen(url, timeout=120) as response:
            if response.status != 200:
                raise RuntimeError("Public health check failed")
            response.read()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--retry", action="store_true", help="Operator-only retry after inspecting/fixing the last failure")
    args = parser.parse_args()
    tag, sha = args.tag, args.commit
    validate_marker({"schema": 1, "repository": REPO, "tag": tag, "commit": sha}, tag, sha)
    os.umask(0o077)
    with (ROOT / "lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (ROOT / "maintenance").exists():
            raise RuntimeError("CD configuration maintenance is incomplete; rerun infra/ansible/cd.yml")
        state = json.loads((ROOT / "state.json").read_text())
        if state.get("attempt") and not args.retry:
            raise RuntimeError("Incomplete deployment; an operator must inspect state.json and deploy.log before --retry")
        release = get_json(f"https://api.github.com/repos/{REPO}/releases/tags/{tag}")
        if release["draft"] or release["tag_name"] != tag or not release["published_at"]:
            raise ValueError("A published release is required")
        marker = get_json(f"https://github.com/{REPO}/releases/download/{tag}/production.json")
        validate_marker(marker, tag, sha)
        source = ROOT / "source"
        if not source.exists():
            run(["git", "clone", "--quiet", "--no-checkout", f"https://github.com/{REPO}.git", str(source)])
        run(["git", "-C", str(source), "fetch", "--quiet", "origin", "tag", tag])
        actual = run(["git", "-C", str(source), "rev-parse", tag + "^{commit}"]).decode().strip()
        if actual != sha:
            raise RuntimeError("Tag moved or marker commit mismatch")
        # Never deploy an older/divergent commit, including a newly republished old tag.
        for base in {state["commit"], state.get("attempt", {}).get("commit", state["commit"])}:
            run(["git", "-C", str(source), "merge-base", "--is-ancestor", base, sha])
        run(["git", "-C", str(source), "checkout", "--quiet", "--detach", sha])
        version = tag[1:]
        if json.loads((source / "sdk/package.json").read_text())["version"] != version:
            raise RuntimeError("Source version mismatch")
        state["attempt"] = state.get("attempt", {}) | {
            "tag": tag, "commit": sha, "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
        write_state(state)
        print("Deploying " + tag, flush=True)
        with (ROOT / "deploy.log").open("wb") as log:
            try:
                saved = backup(version)
                # A retry may back up a partially migrated DB. Preserve the
                # original pre-update backup until the release is verified.
                state["attempt"].setdefault("backup", str(saved))
                write_state(state)
                import yaml
                site = yaml.safe_load((ROOT / "site.yml").read_text())
                site["hibana"]["version"] = version
                desired = ROOT / "desired.yml"
                desired.write_text(yaml.safe_dump(site))
                command = [str(ROOT / "venv/bin/ansible-playbook"), "-i", str(ROOT / "inventory.yml"),
                           "infra/ansible/deploy.yml", "-e", "@" + str(desired), "-e", "@" + str(ROOT / "vault.yml"),
                           "--vault-password-file", str(ROOT / "vault.key")]
                subprocess.run(command, cwd=source, check=True, timeout=2400, stdout=log, stderr=subprocess.STDOUT,
                               env=os.environ | {"ANSIBLE_CONFIG": str(source / "infra/ansible/ansible.cfg")})
                verify(version, site["hibana"])
                desired.replace(ROOT / "site.yml")
                write_state({"tag": tag, "commit": sha, "published_at": release["published_at"],
                             "deployed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "backup": str(saved)})
                # Prune only after success; retain the latest three verified DB/config backups.
                prune_backups()
                print("Production verified: " + tag, flush=True)
            except Exception:
                print("Deployment failed; automatic retries are paused. See private deploy.log.", flush=True)
                raise


if __name__ == "__main__":
    main()
