#!/usr/bin/env python3
"""Render private VPS overlays from reviewed repository manifests. No cluster access."""

import argparse
import hashlib
import hmac
import ipaddress
import json
import os
import re
import shutil
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]


def resource(kind, name, spec=None, namespace=None, api="v1", **extra):
    d = {"apiVersion": api, "kind": kind, "metadata": {"name": name}}
    if namespace:
        d["metadata"]["namespace"] = namespace
    if spec is not None:
        d["spec"] = spec
    return d | extra


def write(path, data):
    text = data if isinstance(data, str) else yaml.safe_dump(data, sort_keys=False)
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.read_text() == text:
        return
    path.write_text(text)
    path.chmod(0o600)


def manifest(path, docs):
    write(path, yaml.safe_dump_all(docs, sort_keys=False))


def kust(resources, **extra):
    return {
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": resources,
    } | extra


def patch(kind, name, spec):
    return {
        "target": {"kind": kind, "name": name},
        "patch": yaml.safe_dump(
            {
                "apiVersion": "apps/v1" if kind == "Deployment" else "v1",
                "kind": kind,
                "metadata": {"name": name},
                "spec": spec,
            }
        ),
    }


def placement(name, role, memory=None, cpu=None, pod_ip=None):
    pod = {"nodeSelector": {"hibana.io/role": role}, "topologySpreadConstraints": None}
    if memory:
        pod["containers"] = [
            {
                "name": "worker" if name == "hibana-worker" else "control-plane",
                "resources": {
                    "requests": {"memory": memory[0], "cpu": cpu},
                    "limits": {"memory": memory[1], "cpu": "2"},
                },
            }
        ]
    if name == "hibana-worker" and memory:
        # The minimum VPS pool uses Recreate: no spare memory for overlapping
        # Workers. A DNS preStop delay cannot bridge that outage; release the
        # old Pod's reservation promptly so its replacement can start.
        pod["containers"][0]["lifecycle"] = None
    template = {"spec": pod}
    if pod_ip:
        template["metadata"] = {
            "annotations": {"cni.projectcalico.org/ipAddrs": json.dumps([pod_ip])}
        }
    return patch(
        "Deployment",
        name,
        {
            "replicas": 1,
            "strategy": {"type": "Recreate", "rollingUpdate": None},
            "template": template,
        },
    )


def ingress(name, hosts, service, port, cfg, secret="hibana-tls"):
    annotations = {
        "traefik.ingress.kubernetes.io/router.entrypoints": "websecure",
        "traefik.ingress.kubernetes.io/router.tls": "true",
    }
    if cfg["tls_mode"] == "acme":
        annotations["traefik.ingress.kubernetes.io/router.tls.certresolver"] = (
            "cloudflare"
        )
    tls = {"hosts": hosts}
    if cfg["tls_mode"] == "provided":
        tls["secretName"] = secret
    d = resource(
        "Ingress",
        name,
        {
            "ingressClassName": "hibana",
            "tls": [tls],
            "rules": [
                {
                    "host": host,
                    "http": {
                        "paths": [
                            {
                                "path": "/",
                                "pathType": "Prefix",
                                "backend": {
                                    "service": {
                                        "name": service,
                                        "port": {"number": port},
                                    }
                                },
                            }
                        ]
                    },
                }
                for host in hosts
            ],
        },
        api="networking.k8s.io/v1",
    )
    d["metadata"]["annotations"] = annotations
    return d


def validate(c, s):
    for key in ["domain", "management_node", "execution_node"]:
        if not re.fullmatch(r"[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?", c[key]):
            raise ValueError(f"Invalid {key}")
    if not c["tenant_slugs"] or len(set(c["tenant_slugs"])) != len(c["tenant_slugs"]):
        raise ValueError("tenant_slugs must be nonempty and unique")
    for tenant in c["tenant_slugs"]:
        if not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", tenant):
            raise ValueError("Invalid tenant slug")
    if c["management_node"] == c["execution_node"] or c["architecture"] not in [
        "amd64",
        "arm64",
    ]:
        raise ValueError(
            "Distinct nodes and a supported image architecture are required"
        )
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?", c["version"]):
        raise ValueError("Use a pinned release version")
    net = ipaddress.ip_network(c["pod_cidr"])
    addresses = [
        ipaddress.ip_address(c[k]) for k in ["ingress_pod_ip", "console_pod_ip"]
    ]
    if len(set(addresses)) != 2 or any(ip not in net for ip in addresses):
        raise ValueError("Proxy IPs must be distinct addresses in the Pod CIDR")
    if ipaddress.ip_address(c["ingress_service_ip"]) in net:
        raise ValueError("Service and Pod addresses must not overlap")
    for k in [
        "db_owner",
        "db_app",
        "redis",
        "s3",
        "oidc",
        "bootstrap",
        "signing",
        "master",
        "keycloak_db",
        "keycloak_admin",
        "owner_password",
    ]:
        if not re.fullmatch(r"[0-9a-f]{64}", s.get(k, "")):
            raise ValueError(f"Missing/invalid credential {k}; use init-secrets.py")
    if c["tls_mode"] not in ["acme", "provided"]:
        raise ValueError("tls_mode must be acme or provided")
    if c["tls_mode"] == "acme" and not s.get("cloudflare_dns_token"):
        raise ValueError("A scoped Cloudflare DNS token is required for ACME")
    if c["tls_mode"] == "provided" and not all(
        s.get(k) for k in ["tls_crt", "tls_key"]
    ):
        raise ValueError("provided TLS requires tls_crt and tls_key PEM contents")


def render(destination, c, s):
    validate(c, s)
    # Initial realm imports and PostgreSQL env vars do not rotate existing credentials.
    # Refuse accidental regeneration or moving retained local volumes to other nodes.
    stable = {
        k: v
        for k, v in s.items()
        if k not in ["cloudflare_dns_token", "tls_crt", "tls_key", "ca_crt"]
    }
    stable.update(
        {
            k: c[k]
            for k in ["domain", "management_node", "execution_node", "admin_email"]
        }
    )
    identity = hashlib.sha256(json.dumps(stable, sort_keys=True).encode()).hexdigest()
    marker = destination / ".identity"
    if marker.exists() and marker.read_text().strip() != identity:
        raise ValueError(
            "Persistent identity/credentials changed. Follow explicit migration/rotation procedures; bootstrap cannot rotate them."
        )
    destination.mkdir(parents=True, exist_ok=True, mode=0o700)
    write(marker, identity + "\n")
    upstream = destination / "upstream"
    shutil.copytree(ROOT / "deploy/kubernetes", upstream, dirs_exist_ok=True)
    domain = c["domain"]
    hosts = {"console": domain, "auth": f"auth.{domain}", "apps": f"apps.{domain}"}
    tag = f"{c['version']}-linux-{c['architecture']}"
    for sub in ["base", "migration"]:
        k = yaml.safe_load((upstream / sub / "kustomization.yaml").read_text())
        k["images"] = [
            {"name": "hibana-platform", "newName": "hibana-platform", "newTag": tag}
        ]
        write(upstream / sub / "kustomization.yaml", k)
    write(upstream / "console/kustomization.yaml", kust(["console.yaml"]))
    site = destination / "site"
    # Migration is still executed by Hibana's existing installation lifecycle.
    shutil.copytree(upstream / "migration", site / "migration", dirs_exist_ok=True)
    secrets = {
        "hibana-runtime": {
            "DATABASE_URL": f"postgres://faas_app:{s['db_app']}@hibana-postgres:5432/hibana",
            "COMPILED_CACHE_KEY": hmac.new(bytes.fromhex(s["signing"]), b"hibana-compiled-cache-key-v1", hashlib.sha256).hexdigest()
        },
        "hibana-control-plane": {
            "REDIS_URL": f"redis://:{s['redis']}@hibana-redis:6379",
            "S3_ACCESS_KEY": "GK" + hashlib.sha256(s["s3"].encode()).hexdigest()[:32],
            "S3_SECRET_KEY": s["s3"],
            "OIDC_CLIENT_SECRET": s["oidc"],
            "BOOTSTRAP_ADMIN_TOKEN": s["bootstrap"],
            "JOB_SIGNING_KEY": s["signing"],
            "SECRETS_MASTER_KEY": s["master"],
        },
        "hibana-migration": {
            "MIGRATION_DATABASE_URL": f"postgres://hibana_admin:{s['db_owner']}@hibana-postgres:5432/hibana"
        },
    }
    generators = []
    for name, values in secrets.items():
        write(site / f"{name}.env", "".join(f"{k}={v}\n" for k, v in values.items()))
        generators.append({"name": name, "envs": [f"{name}.env"]})
    cfg = {
        "S3_ENDPOINT": "http://hibana-objects:9000",
        "S3_BUCKET": "hibana-components",
        "APP_PUBLIC_ORIGIN": f"https://{hosts['apps']}",
        "TRUSTED_PROXY_CIDRS": f"{c['ingress_pod_ip']}/32,{c['console_pod_ip']}/32",
        "OIDC_ISSUER_URL": f"https://{hosts['auth']}/realms/hibana",
        "OIDC_CLIENT_ID": "hibana",
        "OIDC_CALLBACK_URL": f"https://{domain}/api/auth/oidc/callback",
        "OIDC_CONSOLE_URL": f"https://{domain}/",
        "OIDC_SESSION_TTL_SECS": "900",
        "WORKER_MAX_CONCURRENCY": "8",
        "WORKER_GUEST_MEMORY_BUDGET_MIB": "1024",
    }
    patches = [
        placement("hibana-control-plane", "management", ("128Mi", "512Mi"), "100m"),
        placement("hibana-worker", "execution", ("768Mi", "1280Mi"), "500m"),
        placement("hibana-console", "management", pod_ip=c["console_pod_ip"]),
        {
            "target": {"kind": "ConfigMap", "name": "hibana-config"},
            "patch": yaml.safe_dump(resource("ConfigMap", "hibana-config", data=cfg)),
        },
    ]
    for name in ["hibana-control-plane", "hibana-worker"]:
        patches.append(
            {
                "target": {"kind": "PodDisruptionBudget", "name": name},
                "patch": "- op: remove\n  path: /spec/minAvailable\n- op: add\n  path: /spec/maxUnavailable\n  value: 1\n",
            }
        )
    manifest(
        site / "ingress.yaml",
        [
            ingress("hibana-console", [domain], "hibana-console", 8080, c),
            ingress(
                "hibana-apps",
                [f"*.{t}.{hosts['apps']}" for t in c["tenant_slugs"]],
                "hibana-apps",
                8083,
                c,
            ),
        ],
    )
    np = resource(
        "NetworkPolicy",
        "hibana-site-identity-provider",
        {
            "podSelector": {
                "matchLabels": {"app.kubernetes.io/name": "hibana-control-plane"}
            },
            "policyTypes": ["Egress"],
            "egress": [
                {
                    "to": [
                        {
                            "namespaceSelector": {
                                "matchLabels": {
                                    "kubernetes.io/metadata.name": "hibana-edge"
                                }
                            },
                            "podSelector": {"matchLabels": {"app": "traefik"}},
                        }
                    ],
                    "ports": [{"protocol": "TCP", "port": 443}],
                }
            ],
        },
        api="networking.k8s.io/v1",
    )
    manifest(site / "egress.yaml", [np])
    resources = [
        "../upstream/base",
        "../upstream/console",
        "ingress.yaml",
        "egress.yaml",
    ]
    if c["tls_mode"] == "provided":
        tls = resource(
            "Secret",
            "hibana-tls",
            type="kubernetes.io/tls",
            stringData={"tls.crt": s["tls_crt"], "tls.key": s["tls_key"]},
        )
        manifest(site / "tls.yaml", [tls])
        resources.append("tls.yaml")
        write(site / "ca.crt", s.get("ca_crt", s["tls_crt"]))
        generators.append({"name": "hibana-site-ca", "files": ["ca.crt"]})
        patches.append(
            {
                "target": {"kind": "Deployment", "name": "hibana-control-plane"},
                "patch": yaml.safe_dump(
                    resource(
                        "Deployment",
                        "hibana-control-plane",
                        {
                            "template": {
                                "spec": {
                                    "volumes": [
                                        {
                                            "name": "site-ca",
                                            "secret": {"secretName": "hibana-site-ca"},
                                        }
                                    ],
                                    "containers": [
                                        {
                                            "name": "control-plane",
                                            "env": [
                                                {
                                                    "name": "OIDC_CA_CERT_FILE",
                                                    "value": "/etc/hibana-ca/ca.crt",
                                                }
                                            ],
                                            "volumeMounts": [
                                                {
                                                    "name": "site-ca",
                                                    "mountPath": "/etc/hibana-ca",
                                                    "readOnly": True,
                                                }
                                            ],
                                        }
                                    ],
                                }
                            }
                        },
                        api="apps/v1",
                    )
                ),
            }
        )
    write(
        site / "kustomization.yaml",
        kust(
            resources,
            namespace="hibana",
            patches=patches,
            secretGenerator=generators,
            generatorOptions={"disableNameSuffixHash": True},
            images=[
                {"name": "hibana-console", "newName": "hibana-console", "newTag": tag}
            ],
        ),
    )
    # Reuse the PostgreSQL/Redis resources while excluding development MinIO.
    # Only the private copy is changed; existing local installations are untouched.
    excluded = {"hibana-minio", "hibana-minio-data", "hibana-local-bucket"}
    for relative in [
        "local/dependencies/services.yaml",
        "local/dependencies/storage.yaml",
        "local/dependencies/setup.yaml",
        "persistent-dependencies/volumes.yaml",
    ]:
        path = upstream / relative
        docs = [
            d
            for d in yaml.safe_load_all(path.read_text())
            if d and d["metadata"]["name"] not in excluded
        ]
        manifest(path, docs)
    path = upstream / "persistent-dependencies/kustomization.yaml"
    data = yaml.safe_load(path.read_text())
    data["patches"] = [
        p for p in data["patches"] if p["target"]["name"] not in excluded
    ]
    write(path, data)
    dependencies = destination / "dependencies"
    write(dependencies / "garage.yaml", (ROOT / "infra/garage.yaml").read_text())
    write(
        dependencies / "garage.env",
        "\n".join(
            [
                "GARAGE_DEFAULT_ACCESS_KEY="
                + secrets["hibana-control-plane"]["S3_ACCESS_KEY"],
                "GARAGE_DEFAULT_SECRET_KEY=" + s["s3"],
                "GARAGE_DEFAULT_BUCKET=hibana-components",
                "",
            ]
        ),
    )
    # Domain-separated derivations keep independent RPC/metrics credentials stable.
    rpc = hashlib.sha256(("garage-rpc:" + s["s3"]).encode()).hexdigest()
    metrics = hashlib.sha256(("garage-metrics:" + s["s3"]).encode()).hexdigest()
    write(
        dependencies / "garage.toml",
        f'''metadata_dir = "/data/meta"
data_dir = "/data/blocks"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "127.0.0.1:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "{rpc}"
[s3_api]
s3_region = "us-east-1"
api_bind_addr = "[::]:9000"
[admin]
api_bind_addr = "[::]:3903"
metrics_token = "{metrics}"
''',
    )
    write(
        dependencies / "namespace.yaml", (upstream / "base/namespace.yaml").read_text()
    )
    dep_secrets = {
        "POSTGRES_PASSWORD": s["db_owner"],
        "POSTGRES_APP_PASSWORD": s["db_app"],
        "REDIS_PASSWORD": s["redis"],
    }
    write(
        dependencies / "credentials.env",
        "".join(f"{k}={v}\n" for k, v in dep_secrets.items()),
    )
    dep_patches = [
        placement(name, "management" if name == "hibana-redis" else "execution")
        for name in ["hibana-postgres", "hibana-redis"]
    ]
    # The shared namespace enforces Restricted Pod Security. The development
    # dependency templates need explicit non-root identities and volume groups.
    for name, container_name, uid in [
        ("hibana-postgres", "postgres", 999),
        ("hibana-redis", "redis", 999),
    ]:
        container = {
            "name": container_name,
            "securityContext": {
                "allowPrivilegeEscalation": False,
                "capabilities": {"drop": ["ALL"]},
            },
        }
        kind = "Deployment"
        body = resource(
            kind,
            name,
            {
                "template": {
                    "spec": {
                        "securityContext": {
                            "runAsNonRoot": True,
                            "runAsUser": uid,
                            "runAsGroup": uid,
                            "fsGroup": uid,
                            "seccompProfile": {"type": "RuntimeDefault"},
                        },
                        "containers": [container],
                    }
                }
            },
            api="apps/v1",
        )
        dep_patches.append(
            {"target": {"kind": kind, "name": name}, "patch": yaml.safe_dump(body)}
        )
    for name in ["hibana-postgres-data"]:
        dep_patches.append(
            patch(
                "PersistentVolumeClaim",
                name,
                {"storageClassName": "hibana-local", "volumeName": name},
            )
        )
    write(
        dependencies / "kustomization.yaml",
        kust(
            ["namespace.yaml", "../upstream/persistent-dependencies", "garage.yaml"],
            namespace="hibana",
            patches=dep_patches,
            secretGenerator=[
                {"name": "hibana-local-dependencies", "envs": ["credentials.env"]},
                {"name": "hibana-objects", "envs": ["garage.env"]},
                {"name": "hibana-objects-config", "files": ["garage.toml"]},
            ],
            generatorOptions={"disableNameSuffixHash": True},
        ),
    )
    ident = destination / "identity"
    shutil.copytree(ROOT / "deploy/keycloak/kubernetes", ident, dirs_exist_ok=True)
    for file, values in {
        "database.env": {"POSTGRES_PASSWORD": s["keycloak_db"]},
        "bootstrap-admin.env": {
            "KC_BOOTSTRAP_ADMIN_USERNAME": "bootstrap-admin",
            "KC_BOOTSTRAP_ADMIN_PASSWORD": s["keycloak_admin"],
        },
        "client.env": {"HIBANA_OIDC_CLIENT_SECRET": s["oidc"]},
    }.items():
        write(ident / file, "".join(f"{k}={v}\n" for k, v in values.items()))
    # The realm is imported only on an empty identity DB. Existing users are never overwritten.
    realm = json.loads((ident / "realm.example.json").read_text())
    # Give users time to choose a password and complete their first profile.
    realm["accessCodeLifespanUserAction"] = 900
    realm["users"] = [
        {
            "id": s["owner_subject"],
            "username": "owner",
            "email": c["admin_email"],
            "enabled": True,
            # Realm-imported users do not inherit the interactive creation defaults.
            "clientRoles": {"account": ["manage-account", "view-profile"]},
            "requiredActions": ["UPDATE_PASSWORD"],
            "credentials": [
                {"type": "password", "value": s["owner_password"], "temporary": True}
            ],
        }
    ]
    write(ident / "imports/hibana-realm.json", json.dumps(realm, indent=2))
    k = yaml.safe_load((ident / "kustomization.yaml").read_text())
    k["patches"] = [
        placement(n, "management") for n in ["keycloak", "keycloak-postgres"]
    ]
    k["patches"].append(
        patch(
            "Deployment",
            "keycloak-postgres",
            {
                "template": {
                    "spec": {
                        "securityContext": {
                            "runAsNonRoot": True,
                            "runAsUser": 999,
                            "runAsGroup": 999,
                            "fsGroup": 999,
                        },
                        "containers": [
                            {
                                "name": "postgres",
                                "securityContext": {"capabilities": {"drop": ["ALL"]}},
                            }
                        ],
                    }
                }
            },
        )
    )
    k["patches"].append(
        patch(
            "PersistentVolumeClaim",
            "keycloak-postgres",
            {"storageClassName": "hibana-local", "volumeName": "hibana-keycloak-data"},
        )
    )
    keycfg = yaml.safe_load((ident / "config.yaml").read_text())
    keycfg["data"].update(
        KC_HOSTNAME=f"https://{hosts['auth']}",
        KC_PROXY_TRUSTED_ADDRESSES=f"{c['ingress_pod_ip']}/32",
        HIBANA_OIDC_CALLBACK_URL=cfg["OIDC_CALLBACK_URL"],
        HIBANA_OIDC_CONSOLE_URL=cfg["OIDC_CONSOLE_URL"],
    )
    write(ident / "config.yaml", keycfg)
    manifest(
        ident / "ingress.yaml",
        [ingress("keycloak", [hosts["auth"]], "keycloak", 8080, c)],
    )
    if c["tls_mode"] == "provided":
        manifest(ident / "tls.yaml", [tls])
        k["resources"].append("tls.yaml")
    write(ident / "kustomization.yaml", k)
    infrastructure(destination / "infrastructure", c, s)
    print("Rendered private overlays; no cluster or DNS changes were made.")


def infrastructure(path, c, s):
    ns = "hibana-edge"
    docs = [resource("Namespace", ns)]
    docs[0]["metadata"]["labels"] = {
        "hibana.io/ingress": "true",
        "hibana.io/app-ingress": "true",
        "hibana.io/identity-ingress": "true",
    }
    docs += [
        resource(
            "StorageClass",
            "hibana-local",
            api="storage.k8s.io/v1",
            provisioner="kubernetes.io/no-provisioner",
            volumeBindingMode="WaitForFirstConsumer",
            reclaimPolicy="Retain",
        )
    ]
    volumes = [
        ("hibana-keycloak-data", c["management_node"], "identity", "2Gi"),
        ("hibana-postgres-data", c["execution_node"], "postgres", "10Gi"),
        ("hibana-objects-data", c["execution_node"], "objects", "20Gi"),
        ("hibana-traefik-data", c["management_node"], "traefik", "1Gi"),
    ]
    for name, node, directory, size in volumes:
        docs.append(
            resource(
                "PersistentVolume",
                name,
                {
                    "capacity": {"storage": size},
                    "accessModes": ["ReadWriteOnce"],
                    "persistentVolumeReclaimPolicy": "Retain",
                    "storageClassName": "hibana-local",
                    "local": {"path": f"/var/lib/hibana/{directory}"},
                    "nodeAffinity": {
                        "required": {
                            "nodeSelectorTerms": [
                                {
                                    "matchExpressions": [
                                        {
                                            "key": "kubernetes.io/hostname",
                                            "operator": "In",
                                            "values": [node],
                                        }
                                    ]
                                }
                            ]
                        }
                    },
                },
            )
        )
    docs += [
        resource(
            "PersistentVolumeClaim",
            "traefik",
            {
                "storageClassName": "hibana-local",
                "volumeName": "hibana-traefik-data",
                "accessModes": ["ReadWriteOnce"],
                "resources": {"requests": {"storage": "1Gi"}},
            },
            ns,
        ),
        resource("ServiceAccount", "traefik", namespace=ns),
        resource(
            "IngressClass",
            "hibana",
            {"controller": "traefik.io/ingress-controller"},
            api="networking.k8s.io/v1",
        ),
    ]
    rules = [
        {
            "apiGroups": [""],
            "resources": ["services", "secrets"],
            "verbs": ["get", "list", "watch"],
        },
        {
            "apiGroups": ["discovery.k8s.io"],
            "resources": ["endpointslices"],
            "verbs": ["get", "list", "watch"],
        },
        {
            "apiGroups": ["networking.k8s.io"],
            "resources": ["ingresses"],
            "verbs": ["get", "list", "watch"],
        },
        {
            "apiGroups": ["networking.k8s.io"],
            "resources": ["ingresses/status"],
            "verbs": ["update"],
        },
    ]
    docs.append(
        resource(
            "ClusterRole",
            "hibana-traefik-routes",
            api="rbac.authorization.k8s.io/v1",
            rules=rules,
        )
    )
    for target in ["hibana", "hibana-identity"]:
        # Bind only in the two managed namespaces (created before this manifest).
        docs.append(
            resource(
                "RoleBinding",
                "hibana-traefik",
                namespace=target,
                api="rbac.authorization.k8s.io/v1",
                roleRef={
                    "apiGroup": "rbac.authorization.k8s.io",
                    "kind": "ClusterRole",
                    "name": "hibana-traefik-routes",
                },
                subjects=[
                    {"kind": "ServiceAccount", "name": "traefik", "namespace": ns}
                ],
            )
        )
    docs += [
        resource(
            "ClusterRole",
            "hibana-traefik-class",
            api="rbac.authorization.k8s.io/v1",
            rules=[
                {
                    "apiGroups": [""],
                    "resources": ["nodes"],
                    "verbs": ["get", "list", "watch"],
                },
                {
                    "apiGroups": ["networking.k8s.io"],
                    "resources": ["ingressclasses"],
                    "verbs": ["get", "list", "watch"],
                },
            ],
        ),
        resource(
            "ClusterRoleBinding",
            "hibana-traefik-class",
            api="rbac.authorization.k8s.io/v1",
            roleRef={
                "apiGroup": "rbac.authorization.k8s.io",
                "kind": "ClusterRole",
                "name": "hibana-traefik-class",
            },
            subjects=[{"kind": "ServiceAccount", "name": "traefik", "namespace": ns}],
        ),
    ]
    args = [
        "--providers.kubernetesingress=true",
        "--providers.kubernetesingress.ingressclass=hibana",
        "--providers.kubernetesingress.namespaces=hibana,hibana-identity",
        "--entrypoints.web.address=:80",
        "--entrypoints.websecure.address=:443",
        "--entrypoints.web.http.redirections.entrypoint.to=websecure",
        "--entrypoints.web.http.redirections.entrypoint.scheme=https",
        "--ping=true",
        "--entrypoints.traefik.address=:8082",
        "--api.dashboard=false",
        "--log.level=INFO",
    ]
    if c["tls_mode"] == "acme":
        args += [
            "--certificatesresolvers.cloudflare.acme.dnschallenge.provider=cloudflare",
            "--certificatesresolvers.cloudflare.acme.dnschallenge.resolvers=1.1.1.1:53,1.0.0.1:53",
            f"--certificatesresolvers.cloudflare.acme.email={c['acme_email']}",
            f"--certificatesresolvers.cloudflare.acme.caserver={c['acme_server']}",
            "--certificatesresolvers.cloudflare.acme.storage=/data/acme.json",
        ]
    docs.append(
        resource(
            "Secret",
            "cloudflare-dns",
            namespace=ns,
            stringData={"CF_DNS_API_TOKEN": s.get("cloudflare_dns_token", "")},
        )
    )
    pod = {
        "serviceAccountName": "traefik",
        "nodeSelector": {"hibana.io/role": "management"},
        "securityContext": {"runAsUser": 65532, "runAsGroup": 65532, "fsGroup": 65532},
        "containers": [
            {
                "name": "traefik",
                "image": "traefik:v3.7.13",
                "args": args,
                "envFrom": [{"secretRef": {"name": "cloudflare-dns"}}],
                "ports": [
                    {"name": "web", "containerPort": 80, "hostPort": 80},
                    {"name": "websecure", "containerPort": 443, "hostPort": 443},
                    {"name": "ping", "containerPort": 8082},
                ],
                "securityContext": {
                    "allowPrivilegeEscalation": False,
                    "readOnlyRootFilesystem": True,
                    "capabilities": {"drop": ["ALL"], "add": ["NET_BIND_SERVICE"]},
                },
                "resources": {
                    "requests": {"cpu": "50m", "memory": "64Mi"},
                    "limits": {"memory": "192Mi"},
                },
                "readinessProbe": {"httpGet": {"path": "/ping", "port": "ping"}},
                "livenessProbe": {"httpGet": {"path": "/ping", "port": "ping"}},
                "volumeMounts": [
                    {"name": "data", "mountPath": "/data"},
                    {"name": "tmp", "mountPath": "/tmp"},
                ],
            }
        ],
        "volumes": [
            {"name": "data", "persistentVolumeClaim": {"claimName": "traefik"}},
            {"name": "tmp", "emptyDir": {"sizeLimit": "32Mi"}},
        ],
    }
    if c["tls_mode"] == "acme":
        # Kubelet's fsGroup handling adds group access on each PVC mount.
        # Restore ACME's required 0600 mode before Traefik reads its existing keys.
        pod["initContainers"] = [
            {
                "name": "acme-permissions",
                "image": pod["containers"][0]["image"],
                "command": [
                    "sh", "-ec",
                    "umask 077; touch /data/acme.json; chmod 600 /data/acme.json",
                ],
                "securityContext": {
                    "allowPrivilegeEscalation": False,
                    "readOnlyRootFilesystem": True,
                    "capabilities": {"drop": ["ALL"]},
                },
                "resources": {
                    "requests": {"cpu": "10m", "memory": "16Mi"},
                    "limits": {"memory": "64Mi"},
                },
                "volumeMounts": [{"name": "data", "mountPath": "/data"}],
            }
        ]
    docs.append(
        resource(
            "Deployment",
            "traefik",
            {
                "replicas": 1,
                "strategy": {"type": "Recreate"},
                "selector": {"matchLabels": {"app": "traefik"}},
                "template": {
                    "metadata": {
                        "labels": {"app": "traefik"},
                        "annotations": {
                            "cni.projectcalico.org/ipAddrs": json.dumps(
                                [c["ingress_pod_ip"]]
                            ),
                            "hibana.io/dns-credential": hashlib.sha256(
                                s.get("cloudflare_dns_token", "").encode()
                            ).hexdigest(),
                        },
                    },
                    "spec": pod,
                },
            },
            ns,
            api="apps/v1",
        )
    )
    docs.append(
        resource(
            "Service",
            "traefik",
            {
                "clusterIP": c["ingress_service_ip"],
                "selector": {"app": "traefik"},
                "ports": [{"name": "https", "port": 443, "targetPort": 443}],
            },
            ns,
        )
    )
    manifest(path / "resources.yaml", docs)
    write(path / "kustomization.yaml", kust(["resources.yaml"]))
    # Same issuer and valid TLS inside and outside the cluster, without public-IP hairpin NAT.
    corefile = f""".:53 {{
    errors
    health {{
        lameduck 5s
    }}
    ready
    hosts {{
        {c["ingress_service_ip"]} auth.{c["domain"]}
        fallthrough
    }}
    kubernetes cluster.local in-addr.arpa ip6.arpa {{
        pods insecure
        fallthrough in-addr.arpa ip6.arpa
        ttl 30
    }}
    prometheus :9153
    forward . /etc/resolv.conf
    cache 30
    loop
    reload
    loadbalance
}}
"""
    manifest(
        path / "dns.yaml",
        [
            resource(
                "ConfigMap",
                "coredns",
                namespace="kube-system",
                data={"Corefile": corefile},
            )
        ],
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        data = json.load(sys.stdin)
        render(args.output, data["config"], data["secrets"])
    except (ValueError, KeyError) as e:
        sys.exit(str(e))
