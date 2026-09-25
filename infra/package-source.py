#!/usr/bin/env python3
"""Make a reproducible source bundle without caches, credentials or build outputs."""

import gzip
import io
import subprocess
import sys
import tarfile
from pathlib import Path

root = Path(__file__).resolve().parents[1]
files = [root / "infra/render.py", root / "infra/garage.yaml"]
files += sorted((root / "sdk/platform").glob("*.py"))
# Never sweep a local, ignored realm export or private YAML into the upload.
tracked = (
    subprocess.check_output(
        [
            "git",
            "-C",
            str(root),
            "ls-files",
            "-z",
            "--",
            "deploy/kubernetes",
            "deploy/keycloak/kubernetes",
        ]
    )
    .decode()
    .split("\0")
)
files += sorted(
    root / name for name in tracked if name and Path(name).suffix in [".yaml", ".json"]
)
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode="w") as archive:
    for path in files:
        info = archive.gettarinfo(str(path), arcname=str(path.relative_to(root)))
        info.uid = info.gid = info.mtime = 0
        info.uname = info.gname = ""
        info.mode = 0o644
        with path.open("rb") as f:
            archive.addfile(info, f)
content = gzip.compress(buffer.getvalue(), mtime=0)
output = Path(sys.argv[1])
output.parent.mkdir(parents=True, exist_ok=True)
if not output.exists() or output.read_bytes() != content:
    output.write_bytes(content)
    print("changed")
