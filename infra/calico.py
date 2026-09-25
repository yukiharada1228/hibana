"""Render the pinned upstream manifest without BGP or a second network controller."""

import pathlib
import sys

import yaml

source, destination, cidr, peer = sys.argv[1:]
docs = list(yaml.safe_load_all(pathlib.Path(source).read_text()))
for doc in docs:
    if doc.get("kind") == "ConfigMap" and doc["metadata"]["name"] == "calico-config":
        doc["data"]["calico_backend"] = "vxlan"
    if doc.get("kind") == "DaemonSet" and doc["metadata"]["name"] == "calico-node":
        container = doc["spec"]["template"]["spec"]["containers"][0]
        changes = {
            "CALICO_IPV4POOL_CIDR": cidr,
            "CALICO_IPV4POOL_IPIP": "Never",
            "CALICO_IPV4POOL_VXLAN": "Always",
            "IP_AUTODETECTION_METHOD": f"can-reach={peer}",
        }
        container["env"] = [e for e in container["env"] if e["name"] not in changes]
        container["env"] += [{"name": k, "value": v} for k, v in changes.items()]
        container["livenessProbe"]["exec"]["command"] = [
            "/bin/calico-node",
            "-felix-live",
        ]
        container["readinessProbe"]["exec"]["command"] = [
            "/bin/calico-node",
            "-felix-ready",
        ]
path = pathlib.Path(destination)
text = yaml.safe_dump_all(docs, sort_keys=False)
if not path.exists() or path.read_text() != text:
    path.write_text(text)
    print("changed")
