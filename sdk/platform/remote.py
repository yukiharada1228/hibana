#!/usr/bin/env python3
"""Operate an existing Kubernetes installation using an explicit operator identity."""
import argparse
import os
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
    args = parser.parse_args()
    os.umask(0o077)
    try:
        from existing import ExistingCluster
        target = ExistingCluster(args.kubeconfig, args.context, args.overlay, args.image)
        getattr(target, args.action)()
    except ModuleNotFoundError as error:
        if error.name != "yaml":
            raise
        print("Platform operations require Python 3 with PyYAML and kubectl. Application operations require only Node.js.", file=sys.stderr)
        return 1
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
