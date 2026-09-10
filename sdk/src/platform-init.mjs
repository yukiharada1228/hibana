import { cp, mkdir, readdir, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { randomBytes } from "node:crypto";

export async function initPlatform(directory = "hibana-platform") {
  const root = resolve(directory);
  await mkdir(root, { recursive: true, mode: 0o700 });
  if ((await readdir(root)).length) throw new Error(`Directory is not empty: ${root}. Choose a new directory for the platform configuration.`);
  for (const name of ["base", "migration"]) {
    await cp(new URL(`../platform/manifests/${name}/`, import.meta.url), join(root, name), { recursive: true });
  }
  await cp(new URL("../platform/manifests/remote/ingress.yaml", import.meta.url), join(root, "ingress.yaml"));
  const files = {
    "kustomization.yaml": `apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: hibana
resources:
  - base
  - ingress.yaml
  - egress.yaml
patches:
  - path: site.yaml
generatorOptions:
  disableNameSuffixHash: true
secretGenerator:
  - name: hibana-runtime
    envs: [runtime.env]
  - name: hibana-control-plane
    envs: [control-plane.env]
  - name: hibana-migration
    envs: [migration.env]
`,
    "site.yaml": `apiVersion: v1
kind: ConfigMap
metadata:
  name: hibana-config
data:
  S3_ENDPOINT: https://CHANGE_ME
  S3_REGION: us-east-1
  S3_BUCKET: CHANGE_ME
  INGRESS_BASE_DOMAIN: CHANGE_ME
`,
    "egress.yaml": `# Use the address ranges and ports of your external dependencies.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: hibana-site-dependencies
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/part-of: hibana
  policyTypes: [Egress]
  egress:
    - to: [{ipBlock: {cidr: CHANGE_ME_POSTGRES_CIDR}}]
      ports: [{protocol: TCP, port: 5432}]
    - to: [{ipBlock: {cidr: CHANGE_ME_REDIS_CIDR}}]
      ports: [{protocol: TCP, port: 6379}]
    - to: [{ipBlock: {cidr: CHANGE_ME_S3_CIDR}}]
      ports: [{protocol: TCP, port: 443}]
`,
    "runtime.env": "DATABASE_URL=postgres://CHANGE_ME\n",
    "control-plane.env": `REDIS_URL=redis://CHANGE_ME
S3_ACCESS_KEY=CHANGE_ME
S3_SECRET_KEY=CHANGE_ME
BOOTSTRAP_ADMIN_TOKEN=${randomBytes(32).toString("hex")}
JOB_SIGNING_KEY=${randomBytes(32).toString("hex")}
SECRETS_MASTER_KEY=${randomBytes(32).toString("hex")}
`,
    "migration.env": "MIGRATION_DATABASE_URL=postgres://CHANGE_ME\n",
    ".gitignore": "*.env\n*.pem\n*.key\n",
    "README.md": `# Hibana site configuration

1. Fill in runtime.env, control-plane.env and migration.env with your PostgreSQL, Redis and S3 credentials. Keep the generated signing, master and bootstrap keys; back them up securely.
2. Update site.yaml with your S3 endpoint, bucket and application domain.
3. Update ingress.yaml with your management hostname, per-tenant app hostname, IngressClass and TLS Secret names. Provision the TLS Secrets in namespace hibana or include them as resources in this overlay. Label the Ingress controller namespace as described in ingress.yaml.
4. Set your dependency address ranges and ports in egress.yaml. Provision the external databases and bucket before installation.
5. Run the preview, resolve every reported issue, then install:

\`\`\`sh
hibana platform install --kubeconfig /path/to/config --context onprem --overlay . --image registry.example.com/hibana/platform:VERSION --dry-run
hibana platform install --kubeconfig /path/to/config --context onprem --overlay . --image registry.example.com/hibana/platform:VERSION
\`\`\`

The preview checks cluster permissions, required configuration and references, and displays resource changes without secret values. Namespaced server validation requires an existing namespace; on first installation it runs after namespace creation.

Migration credentials must have migration privileges; application credentials should use the restricted application role. A fresh platform also needs a tenant account created by its administrator before application login. Refer to the Hibana on-premises guide for tenant provisioning and backup/restore procedures.

If installation fails, follow the reported inspection and retry commands. Failed or running migration Jobs are preserved. After diagnosing and correcting a failed migration, explicitly remove that Job before retrying; do not delete the namespace or the backing data.

Python HTTPS checks use the system CA trust. For a private CA, configure SSL_CERT_FILE before installation.
`,
  };
  for (const [name, contents] of Object.entries(files)) {
    await writeFile(join(root, name), contents, { flag: "wx", mode: name.endsWith(".env") ? 0o600 : 0o644 });
  }
  console.log(`Created platform configuration: ${root}\nFill in the site settings listed in README.md, then run hibana platform install with --overlay pointing to this directory.\nSigning, encryption and bootstrap keys were generated in control-plane.env (excluded from Git).`);
}
