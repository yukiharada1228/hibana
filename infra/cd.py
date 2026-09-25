#!/usr/bin/env python3
"""Pull only CI-approved public releases. Runs on the control plane, never in a PR."""

import argparse
import datetime
import fcntl
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
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


def candidate(releases, state):
    eligible = [r for r in releases if not r["draft"]
                and r["published_at"] > state["published_at"]
                and any(a["name"] == "production.json" for a in r["assets"])]
    return max(eligible, key=lambda r: r["published_at"], default=None)


def write_state(state):
    temporary = ROOT / "state.tmp"
    temporary.write_text(json.dumps(state, indent=2) + "\n")
    temporary.replace(ROOT / "state.json")


def backup(version):
    from ansible.parsing.vault import VaultLib, VaultSecret

    files = {}
    for namespace in ["hibana", "hibana-identity", "hibana-edge"]:
        files[namespace + ".json"] = run(KUBE + ["-n", namespace, "get",
            "deployments,configmaps,secrets,services,ingresses,networkpolicies,persistentvolumeclaims", "-o", "json"])
    files["rendered.tar.gz"] = run(["tar", "-C", "/opt/hibana", "-czf", "-", "rendered"])
    files["site.yml"] = (ROOT / "site.yml").read_bytes()
    for database, namespace, deployment, user in [
        ("hibana", "hibana", "hibana-postgres", "hibana_admin"),
        ("keycloak", "hibana-identity", "keycloak-postgres", "keycloak"),
    ]:
        command = KUBE + ["-n", namespace, "exec", "deployment/" + deployment, "--"]
        data = run(command + ["pg_dump", "-U", user, "-d", database, "-Fc"])
        if not data.startswith(b"PGDMP"):
            raise RuntimeError("Invalid database backup")
        run(KUBE + ["-n", namespace, "exec", "-i", "deployment/" + deployment,
                    "--", "pg_restore", "--list"], input=data)
        files[database + ".dump"] = data
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(data), 0o600
            archive.addfile(info, io.BytesIO(data))
    vault = VaultLib([("default", VaultSecret((ROOT / "vault.key").read_bytes().strip()))])
    encrypted = vault.encrypt(buffer.getvalue())
    if vault.decrypt(encrypted) != buffer.getvalue():
        raise RuntimeError("Backup encryption verification failed")
    directory = ROOT / "backups"
    directory.mkdir(exist_ok=True)
    target = directory / (datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + version + ".vault")
    target.write_bytes(encrypted)
    return target


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
    parser.add_argument("--retry", action="store_true", help="Operator-only retry after inspecting/fixing the last failure")
    args = parser.parse_args()
    os.umask(0o077)
    with (ROOT / "lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state = json.loads((ROOT / "state.json").read_text())
        if state.get("attempt") and not args.retry:
            print("CD paused after an incomplete attempt; inspect state.json and deploy.log.")
            return
        release = candidate(get_json(f"https://api.github.com/repos/{REPO}/releases?per_page=20"), state)
        if not release:
            return
        tag = release["tag_name"]
        # Validate before using the tag in URLs or git arguments.
        if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?", tag):
            raise ValueError("Invalid release tag")
        marker = get_json(f"https://github.com/{REPO}/releases/download/{tag}/production.json")
        sha = marker.get("commit", "")
        validate_marker(marker, tag, sha)
        source = ROOT / "source"
        if not source.exists():
            run(["git", "clone", "--quiet", "--no-checkout", f"https://github.com/{REPO}.git", str(source)])
        run(["git", "-C", str(source), "fetch", "--quiet", "origin", "tag", tag])
        actual = run(["git", "-C", str(source), "rev-parse", tag + "^{commit}"]).decode().strip()
        if actual != sha:
            raise RuntimeError("Tag moved or marker commit mismatch")
        # Never auto-deploy an older/divergent commit, including a newly republished old tag.
        run(["git", "-C", str(source), "merge-base", "--is-ancestor", state["commit"], sha])
        run(["git", "-C", str(source), "checkout", "--quiet", "--detach", sha])
        version = tag[1:]
        if json.loads((source / "sdk/package.json").read_text())["version"] != version:
            raise RuntimeError("Source version mismatch")
        state["attempt"] = {"tag": tag, "commit": sha, "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
        write_state(state)
        print("Deploying " + tag, flush=True)
        with (ROOT / "deploy.log").open("wb") as log:
            try:
                saved = backup(version)
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
                for old in sorted((ROOT / "backups").glob("*.vault"))[:-3]:
                    old.unlink()
                print("Production verified: " + tag, flush=True)
            except Exception:
                print("Deployment failed; automatic retries are paused. See private deploy.log.", flush=True)
                raise


if __name__ == "__main__":
    main()
