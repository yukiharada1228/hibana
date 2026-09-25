#!/usr/bin/env python3
"""Emit Ansible inventory for these three disposable Lima VMs only."""

import json
import pathlib
import subprocess

import yaml

root = pathlib.Path(__file__).resolve().parents[2]
state = root / ".local/iac"
state.mkdir(parents=True, exist_ok=True)
groups = {}
(state / "known_hosts").write_text("")
instances = {
    d["name"]: d
    for d in [
        json.loads(l)
        for l in subprocess.check_output(
            ["limactl", "list", "--json"], text=True
        ).splitlines()
    ]
}
for role in ["cp", "management", "execution"]:
    name = f"hibana-iac-{role}"
    addresses = json.loads(
        subprocess.check_output(
            ["limactl", "shell", name, "ip", "-4", "-j", "addr", "show", "eth0"]
        )
    )
    ip = addresses[0]["addr_info"][0]["local"]
    user = subprocess.check_output(
        ["limactl", "shell", name, "whoami"], text=True
    ).strip()
    # Verify the actual guest key through Lima, then pin it for direct SSH.
    key = subprocess.check_output(
        ["limactl", "shell", name, "cat", "/etc/ssh/ssh_host_ed25519_key.pub"],
        text=True,
    ).strip()
    with (state / "known_hosts").open("a") as f:
        f.write(f"[127.0.0.1]:{instances[name]['sshLocalPort']} {key}\n")
    groups["control_plane" if role == "cp" else role] = {
        "hosts": {
            name: {
                "ansible_host": "127.0.0.1",
                "ansible_port": instances[name]["sshLocalPort"],
                "node_ip": ip,
                "ansible_user": user,
                "ansible_ssh_private_key_file": str(
                    pathlib.Path.home() / ".lima/_config/user"
                ),
            }
        }
    }
groups["workers"] = {"children": {"management": {}, "execution": {}}}
inv = {
    "all": {
        "vars": {
            "ansible_python_interpreter": "/usr/bin/python3",
            "ansible_ssh_common_args": f"-o UserKnownHostsFile={state / 'known_hosts'} -o StrictHostKeyChecking=yes",
            "kubernetes_version": "1.36.5",
            "pod_cidr": "10.244.0.0/16",
            "service_cidr": "10.96.0.0/16",
            "ssh_source_cidrs": ["192.168.106.1/32", "192.168.106.2/32"],
        },
        "children": groups,
    }
}
(state / "inventory.yml").write_text(yaml.safe_dump(inv))
print(state / "inventory.yml")
