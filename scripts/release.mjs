// Assemble public release files from explicit inputs; never copy a checkout wholesale.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmod, copyFile, mkdir, readFile, readdir, rename, stat, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const pkg = JSON.parse(await readFile(join(root, "sdk/package.json"), "utf8"));
const lock = JSON.parse(await readFile(join(root, "sdk/package-lock.json"), "utf8"));
const cargo = await readFile(join(root, "Cargo.toml"), "utf8");
const version = pkg.version;
assert.match(version, /^\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?$/);
assert.equal(/\[workspace.package\][\s\S]*?version\s*=\s*"([^"]+)"/.exec(cargo)?.[1], version, "CLI and runtime versions differ");
assert.equal(lock.packages[""].version, version, "npm lock version differs");
assert.equal(lock.packages[""].name, pkg.name, "npm lock name differs");
assert.notEqual(pkg.private, true, "The CLI must be publishable to npm");
assert.equal(pkg.publishConfig.access, "public");
assert.equal(pkg.publishConfig.registry, "https://registry.npmjs.org");
assert.equal(JSON.parse(await readFile(join(root, "console/package.json"), "utf8")).version, version, "Console and platform versions differ");
if (process.env.GITHUB_REF_TYPE === "tag") assert.equal(process.env.GITHUB_REF_NAME, `v${version}`, "Release tag and package version differ");

const targets = ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"];
const [command, ...args] = process.argv.slice(2);
if (command === "version" && !args.length) console.log(version);
else if (command === "cli" && args.length === 1) {
  const output = resolve(args[0]); await mkdir(output, { recursive: true });
  const [packed] = JSON.parse(execFileSync("npm", ["pack", "--json", "--pack-destination", output],
    { cwd: join(root, "sdk"), encoding: "utf8" }));
  assert.equal(packed.name, pkg.name);
  assert.equal(packed.version, version);
  // Keep GitHub/offline asset names independent of the npm account scope.
  const source = join(output, packed.filename);
  const destination = join(output, `hibana-cli-${version}.tgz`);
  if (source !== destination) await rename(source, destination);
  console.log(destination);
}
else if (command === "runtime" && args.length === 2) {
  const [binary, output] = args.map(arg => resolve(arg));
  const target = `${process.platform}-${process.arch}`;
  assert.ok(targets.includes(target), `Unsupported runtime target: ${target}`);
  assert.equal(execFileSync(binary, ["--version"], { encoding: "utf8", timeout: 10000 }).trim(), `hibana-worker ${version}`);
  await mkdir(output, { recursive: true });
  const destination = join(output, `hibana-worker-${version}-${target}`);
  await copyFile(binary, destination); await chmod(destination, 0o755);
  console.log(destination);
} else if (command === "platform" && args.length === 1) {
  const output = resolve(args[0]); await mkdir(output, { recursive: true });
  const destination = join(output, `hibana-kubernetes-${version}.tar.gz`);
  execFileSync("tar", ["-czf", destination, "LICENSE", "deploy/kubernetes/README.md", "deploy/kubernetes/base", "deploy/kubernetes/remote", "deploy/kubernetes/console", "deploy/kubernetes/migration", "deploy/kubernetes/autoscaling", "deploy/keycloak", "docs/authentication.md", "docs/saml-keycloak.md", "docs/security.md", "docs/resilience.md", "docs/deployment.md", "docs/console.md", "docs/application-logs.md", "docs/database.md", "docs/remote-cli.md", "docs/on-prem-production.md", "docs/releases.md", "docs/release-candidate.md"], { cwd: root, timeout: 30000 });
  console.log(destination);
} else if (command === "checksums" && (args.length === 1 || (args.length === 2 && args[1] === "--complete"))) {
  const directory = resolve(args[0]);
  const expected = [...targets.map(target => `hibana-worker-${version}-${target}`), `hibana-cli-${version}.tgz`, `hibana-kubernetes-${version}.tar.gz`, ...["platform", "console"].flatMap(image => ["amd64", "arm64"].map(arch => `hibana-${image}-${version}-linux-${arch}.tar`))];
  const names = (await readdir(directory)).filter(name => name !== "SHA256SUMS").sort();
  assert.ok(names.length > 0, "No release files");
  for (const name of names) assert.ok(expected.includes(name), `Unexpected release file: ${name}`);
  if (args[1]) assert.deepEqual(names, expected.sort(), "Release is missing a platform or package");
  const lines = [];
  for (const name of names) {
    assert.ok((await stat(join(directory, name))).isFile());
    // Stream large image archives instead of allocating their full size.
    const { createReadStream } = await import("node:fs");
    const hash = createHash("sha256");
    for await (const chunk of createReadStream(join(directory, name))) hash.update(chunk);
    lines.push(`${hash.digest("hex")}  ${name}`);
  }
  await writeFile(join(directory, "SHA256SUMS"), lines.join("\n") + "\n");
  console.log(`Checksummed ${names.length} release files${args[1] ? " (complete matrix)" : " (available local targets only)"}.`);
} else throw new Error("Usage: node scripts/release.mjs version | cli OUTPUT | runtime BINARY OUTPUT | platform OUTPUT | checksums OUTPUT [--complete]");
