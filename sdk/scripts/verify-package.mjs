// Exercise the actual npm tarball outside the source tree. No platform is started.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { createServer as createRegistry } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve, delimiter } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { cliRelease, releaseBase } from "../src/package.mjs";

const sdk = fileURLToPath(new URL("../", import.meta.url));
const temporary = await mkdtemp(join(tmpdir(), "hibana-package-"));
const env = { ...process.env, HIBANA_CONFIG_HOME: join(temporary, "profiles"), HIBANA_RUNTIME_HOME: join(temporary, "runtimes"), HIBANA_RUNTIME_BIN: "", HIBANA_PROFILE: "", HIBANA_URL: "", HIBANA_TOKEN: "" };
let registry;
async function run(command, args, cwd = temporary, overrides = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd, env: { ...env, ...overrides }, stdio: ["ignore", "pipe", "pipe"] });
    let output = "", errors = "";
    child.stdout.on("data", b => output += b); child.stderr.on("data", b => errors += b);
    child.once("error", reject);
    child.once("exit", code => code === 0 ? resolve(output) : reject(new Error(`${command} ${args.join(" ")}: ${code}\n${output}\n${errors}`)));
  });
}

try {
  const github = Boolean(process.env.HIBANA_GITHUB_RELEASE);
  const metadata = JSON.parse(await readFile(join(sdk, "package.json"), "utf8"));
  const release = cliRelease(metadata.version);
  const packed = JSON.parse(await run("npm", ["pack", "--json", "--pack-destination", temporary, ...(github ? ["--ignore-scripts", release] : [])], sdk))[0];
  assert.equal(packed.version, metadata.version);
  const paths = packed.files.map(f => f.path);
  assert.ok(paths.includes("platform/network.py"), "platform/network.py");
  assert.ok(paths.includes("platform/operation.py"), "platform/operation.py");
  assert.ok(paths.includes("platform/readiness.py"), "platform/readiness.py");
  assert.ok(paths.includes("src/extension-manifest.mjs"), "src/extension-manifest.mjs");
  for (const required of ["LICENSE", "src/cli.mjs", "src/runtime.mjs", "src/profiles.mjs", "platform/remote.py", "platform/common.py", "platform/maintenance.py", "platform/preflight.py", "platform/existing.py", "platform/manifests/base/kustomization.yaml", "platform/manifests/remote/ingress.yaml", "platform/manifests/migration/job.yaml", "templates/hono/src/index.ts", "wit/world.wit"]) assert.ok(paths.includes(required), required);
  assert.ok(paths.every(path => !/^(examples|test|node_modules)\/|kubernetes\.py$|\.hibana|\.env$|Dockerfile|Cargo\.toml/.test(path) || path === "templates/rust/Cargo.toml"));
  const tarball = join(temporary, packed.filename);
  if (github) {
    const response = await fetch(releaseBase(metadata.version) + "SHA256SUMS", { signal: AbortSignal.timeout(30000) });
    assert.equal(response.status, 200);
    const { releaseChecksum } = await import("../src/runtime.mjs");
    assert.equal(createHash("sha256").update(await readFile(tarball)).digest("hex"), releaseChecksum(await response.text(), `hibana-cli-${metadata.version}.tgz`));
  }
  // Serve only this candidate through an isolated scoped registry. The exact
  // npx command must work before public release, without a global hibana binary.
  const bytes = await readFile(tarball);
  registry = createRegistry((req, res) => {
    const path = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
    if (path === "/cli.tgz") {
      res.setHeader("Content-Type", "application/octet-stream");
      return res.end(bytes);
    }
    if (path !== `/${metadata.name}`) { res.writeHead(404); return res.end(); }
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({name: metadata.name, "dist-tags": {latest: metadata.version}, versions: {
      [metadata.version]: {...metadata, dist: {
        tarball: `http://127.0.0.1:${registry.address().port}/cli.tgz`,
        integrity: `sha512-${createHash("sha512").update(bytes).digest("base64")}`,
      }},
    }}));
  });
  await new Promise(resolve => registry.listen(0, "127.0.0.1", resolve));
  // A previous run may have cached a different candidate with the same version.
  env.npm_config_cache = join(temporary, "npm-cache");
  env.npm_config_userconfig = join(temporary, ".npmrc");
  await writeFile(env.npm_config_userconfig, `${metadata.name.split("/")[0]}:registry=http://127.0.0.1:${registry.address().port}\n`);
  const guard = join(temporary, "bin"); await mkdir(guard);
  await writeFile(join(guard, "hibana"), '#!/bin/sh\necho "Unexpected global hibana dependency" >&2\nexit 99\n', {mode: 0o755});
  env.PATH = [guard, process.env.PATH].filter(Boolean).join(delimiter);
  const installation = join(temporary, "installation"); await mkdir(installation);
  const npmFlags = ["--no-audit", "--no-fund", ...(process.env.HIBANA_PACKAGE_OFFLINE ? ["--offline"] : [])];
  await run("npm", ["install", "--omit=optional", "--ignore-scripts", ...npmFlags, github ? release : tarball], installation);
  const cli = join(installation, "node_modules", metadata.name, "src/cli.mjs");
  assert.equal((await run(process.execPath, [cli, "--version"])).trim(), `hibana ${packed.version}`);
  assert.match(await run(process.execPath, [cli, "--help"]), /--profile/);
  console.log("Packed CLI installed without JS compilers, Docker, platform source or a local runtime.");
  console.log(await run(process.execPath, ["--test", join(sdk, "test/remote.test.mjs"), join(sdk, "test/dev.test.mjs"), join(sdk, "test/cli.test.mjs")], temporary, { HIBANA_TEST_CLI: cli }));
  assert.match(await run(process.execPath, [cli, "platform", "install", "--help"]), /--kubeconfig/);

  const project = join(temporary, "hello");
  const npx = ["--yes", `${metadata.name}@${metadata.version}`];
  assert.equal((await run("npx", [...npx, "--version"])).trim(), `hibana ${metadata.version}`);
  await run("npx", [...npx, "init", project, "--template", "hono", "--no-install"]);
  const projectPackage = JSON.parse(await readFile(join(project, "package.json"), "utf8"));
  assert.deepEqual(projectPackage.devDependencies, { [metadata.name]: metadata.version });
  assert.deepEqual(projectPackage.scripts, { dev: "hibana dev", build: "hibana build", deploy: "hibana deploy" });
  assert.deepEqual(Object.keys(projectPackage.dependencies), ["hono"]);
  await run("npm", ["install", ...npmFlags], project);
  const projectCli = join(project, "node_modules", metadata.name, "src/cli.mjs");
  assert.equal((await run(process.execPath, [projectCli, "--version"])).trim(), `hibana ${metadata.version}`);
  const lock = JSON.parse(await readFile(join(project, "package-lock.json"), "utf8"));
  assert.equal(lock.packages[`node_modules/${metadata.name}`].version, metadata.version);
  await writeFile(join(guard, "npx"), '#!/bin/sh\necho "Unexpected npx dependency in project scripts" >&2\nexit 99\n', {mode: 0o755});
  await run("npm", ["run", "build"], project, { npm_config_offline: "true" });
  const wasm = await readFile(join(project, ".hibana/build/app.wasm"));
  assert.deepEqual(wasm.subarray(0, 8), Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]));
  console.log(`npx init installed a pinned project-local CLI; npm run build succeeded without npx or a global CLI: ${wasm.length}-byte Wasm Component (SHA-256 ${createHash("sha256").update(wasm).digest("hex")}).`);

  if (process.env.HIBANA_RUNTIME_BIN || github) {
    const listener = createServer();
    await new Promise(resolve => listener.listen(0, "127.0.0.1", resolve));
    const port = listener.address().port;
    await new Promise(resolve => listener.close(resolve));
    if (!github) {
      const runtime = resolve(process.env.HIBANA_RUNTIME_BIN);
      assert.equal((await run(runtime, ["--version"])).trim(), `hibana-worker ${packed.version}`);
      const checksum = createHash("sha256").update(await readFile(runtime)).digest("hex");
      await run(process.execPath, [cli, "runtime", "install", "--from", runtime, "--sha256", checksum]);
    }
    const managedRuntime = join(env.HIBANA_RUNTIME_HOME, packed.version, `${process.platform}-${process.arch}`, "hibana-worker");
    const child = spawn(process.execPath, [projectCli, "dev", "--no-watch", "--port", String(port)], { cwd: project, env, stdio: ["ignore", "pipe", "pipe"] });
    let output = ""; child.stdout.on("data", b => output += b); child.stderr.on("data", b => output += b);
    const exited = new Promise(resolve => child.once("exit", (code, signal) => resolve({ code, signal })));
    child.once("error", error => { output += error.message; });
    try {
      const deadline = Date.now() + (github ? 300000 : 120000);
      let response;
      while (Date.now() < deadline && child.exitCode === null) {
        try { response = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(2000) }); break; } catch {}
        await new Promise(resolve => setTimeout(resolve, 200));
      }
      assert.equal(response?.status, 200, output);
      assert.match(await response.text(), /Hello from Hono on Hibana/);
      assert.equal((await run(managedRuntime, ["--version"])).trim(), `hibana-worker ${packed.version}`);
      if (github) assert.match(output, /Downloading the runtime matching this CLI version/);
      console.log("Hono response verified on the checksum-installed Wasmtime runtime, discovered without --runtime or PATH changes.");
    } finally {
      child.kill("SIGINT");
      const timer = setTimeout(() => child.kill("SIGKILL"), 70000);
      const result = await exited; clearTimeout(timer);
      assert.equal(result.code, 0, output);
    }
    await assert.rejects(fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(2000) }));
    console.log("Ctrl+C stopped the runtime and closed its HTTP listener.");
  } else console.log("Runtime execution skipped: set HIBANA_RUNTIME_BIN to verify Wasmtime execution and shutdown.");
  console.log("Standalone package verification passed.");
} finally {
  if (registry) { registry.closeAllConnections(); await new Promise(resolve => registry.close(resolve)); }
  await rm(temporary, { recursive: true, force: true });
}
