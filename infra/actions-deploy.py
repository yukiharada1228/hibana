#!/usr/bin/python3
"""Root-owned forced SSH command. Accept only a release tag and exact commit."""

import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path("/opt/hibana/cd")


def parse_command(arguments):
    if len(arguments) != 1:
        raise ValueError("Expected one deployment command")
    match = re.fullmatch(r"deploy (v\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?) ([a-f0-9]{40})", arguments[0])
    if not match:
        raise ValueError("Only deploy <release-tag> <commit-sha> is permitted")
    return match.groups()


def main():
    tag, sha = parse_command(sys.argv[1:])
    print("Starting verified production deployment: " + tag, flush=True)
    # The transient unit continues if SSH/GitHub disconnects. --wait propagates
    # its actual exit status; a fixed unit name plus cd.py's flock serializes work.
    result = subprocess.run([
        "systemd-run", "--quiet", "--wait", "--collect", "--unit=hibana-deploy",
        "--property=Type=exec", "--property=RuntimeMaxSec=50min", "--property=UMask=0077",
        "--property=MemoryMax=512M", "--property=Nice=10",
        str(ROOT / "venv/bin/python"), str(ROOT / "cd.py"), "--tag", tag, "--commit", sha,
    ], check=False)
    if result.returncode:
        print("Deployment failed or another deployment is active. Inspect the VPS deployment log.", file=sys.stderr)
        return result.returncode
    state = json.loads((ROOT / "state.json").read_text())
    if state.get("attempt") or state.get("tag") != tag or state.get("commit") != sha:
        raise RuntimeError("Deployment state does not confirm the requested release")
    print(json.dumps({k: state[k] for k in ["tag", "commit", "deployed_at"]}), flush=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, RuntimeError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
