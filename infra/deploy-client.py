#!/usr/bin/env python3
"""Invoke the forced production command over pinned SSH; never upload site secrets."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def ssh_arguments(directory, host, port, tag, commit):
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9.-]*", host):
        raise ValueError("Invalid deployment host")
    if not 1024 <= int(port) <= 65535:
        raise ValueError("Invalid deployment port")
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?", tag):
        raise ValueError("Invalid release tag")
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise ValueError("Invalid release commit")
    return ["ssh", "-F", "/dev/null", "-T", "-p", str(port), "-i", str(directory / "key"),
            "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "GlobalKnownHostsFile=/dev/null", "-o", "UserKnownHostsFile=" + str(directory / "known_hosts"),
            "-o", "HostKeyAlgorithms=ssh-ed25519", "-o", "ConnectTimeout=20",
            "-o", "ServerAliveInterval=20", "-o", "ServerAliveCountMax=6",
            "hibana-deploy@" + host, "deploy " + tag + " " + commit]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hibana-deploy-", dir=os.environ.get("RUNNER_TEMP")) as temporary:
        directory = Path(temporary)
        command = ssh_arguments(directory, os.environ["PRODUCTION_SSH_HOST"],
                                os.environ["PRODUCTION_SSH_PORT"], args.tag, args.commit)
        for name, variable in [("key", "PRODUCTION_SSH_KEY"), ("known_hosts", "PRODUCTION_SSH_KNOWN_HOSTS")]:
            value = os.environ[variable].strip()
            if not value:
                raise ValueError("Missing " + variable)
            path = directory / name
            path.touch(mode=0o600)
            path.write_text(value + "\n")
        subprocess.run(command, check=True, timeout=51 * 60)


if __name__ == "__main__":
    main()
