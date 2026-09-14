#!/usr/bin/env python3
"""Operate an existing Kubernetes installation using an explicit operator identity."""
import argparse
import os
import shlex
import signal
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["install", "start", "stop", "status", "uninstall"])
    parser.add_argument("--kubeconfig", required=True, type=Path)
    parser.add_argument("--context", required=True)
    parser.add_argument("--overlay", type=Path)
    parser.add_argument("--image")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    os.umask(0o077)
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    target = None
    try:
        from existing import ExistingCluster
        target = ExistingCluster(args.kubeconfig, args.context, args.overlay, args.image)
        if args.dry_run:
            target.preview(args.action)
        else:
            getattr(target, args.action)()
    except ModuleNotFoundError as error:
        if error.name != "yaml":
            raise
        print("Platform operations require Python 3 with PyYAML and kubectl. Application operations require only Node.js.", file=sys.stderr)
        return 1
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"Error: {error}", file=sys.stderr)
        phase = getattr(target, "install_phase", "preflight") if args.action == "install" else args.action
        print(f"Stopped during: {phase}", file=sys.stderr)
        kubectl = ["kubectl", "--kubeconfig", str(args.kubeconfig), "--context", args.context, "-n", "hibana"]
        print("Inspect: " + shlex.join([*kubectl, "get", "pods,jobs,events"]), file=sys.stderr)
        if phase == "database migration" or "Migration has not succeeded" in str(error):
            print("Migration: " + shlex.join([*kubectl, "describe", "job/hibana-migrate"]), file=sys.stderr)
            print("The migration Job was retained. Diagnose it before explicitly removing a failed Job and retrying.", file=sys.stderr)
        retry = ["hibana", "platform", args.action, "--kubeconfig", str(args.kubeconfig), "--context", args.context]
        if args.overlay:
            retry += ["--overlay", str(args.overlay)]
        if args.image:
            retry += ["--image", args.image]
        if args.dry_run:
            retry.append("--dry-run")
        print("Retry after fixing the issue: " + shlex.join(retry), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Interrupted. Rerun the same command to resume after inspecting any pending migration Job.", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
