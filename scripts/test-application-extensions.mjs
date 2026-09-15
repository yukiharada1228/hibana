// Prerequisites: npm ci in sdk and sdk/examples/hono-extensions, npm run build
// in the example's extension directory, wac on PATH, release hibana-worker and
// debug hibana-control-plane built.
// Uses only a temporary directory and loopback HTTP; no platform DB or credentials.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { createHash } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  copyFile,
  readFile,
  writeFile,
  rm,
  access,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { build } from "../sdk/src/build.mjs";
import { compileJavaScript } from "../sdk/src/javascript.mjs";
import {
  resolveExtensions,
  prepareExtensionWit,
} from "../sdk/src/extensions.mjs";
import { HTTP_CONTRACT } from "../sdk/src/extension-manifest.mjs";
import { loadConfig } from "../sdk/src/config.mjs";
import { deploy } from "../sdk/src/api.mjs";
import { runCommand } from "./bounded-process.mjs";

assert.equal(
  process.argv.length,
  2,
  "This test always builds and runs the full acceptance suite",
);
const validate = async (path) =>
  JSON.parse(
    await runCommand(
      resolve(
        process.env.HIBANA_TEST_CP_BIN || "target/debug/hibana-control-plane",
      ),
      ["--validate-stdin"],
      { input: await readFile(path) },
    ),
  );

const folder = await mkdtemp(join(tmpdir(), "hibana-app-extensions-"));
const settings = join(folder, "settings.json");
const worker = resolve(
  process.env.HIBANA_TEST_RUNTIME_BIN || "target/release/hibana-worker",
);
const workerArgs = (component) => [
  "--dev-component",
  component,
  "--dev-settings",
  settings,
  "--bind",
  "127.0.0.1:0",
];
let child;
let logs = "";
const stop = async () => {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null)
    return;
  const stopped = new Promise((done) => child.once("exit", done));
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 3000);
  await stopped;
  clearTimeout(timer);
};
try {
  // Install the actual distribution outside the checkout. Consumers need no
  // Rust compiler, author build scripts, manual WIT or platform source changes.
  const example = resolve("sdk/examples/hono-extensions");
  const packed = JSON.parse(
    await runCommand(
      "npm",
      ["pack", "--ignore-scripts", "--json", "--pack-destination", folder],
      { cwd: join(example, "extension") },
    ),
  )[0];
  assert.ok(packed.files.some((file) => file.path === "dist/crypto.wasm"));
  assert.ok(packed.files.some((file) => file.path === "wit/world.wit"));
  assert.ok(
    packed.files.every(
      (file) =>
        !file.path.startsWith("rust-crypto/") && file.path !== "build.mjs",
    ),
  );
  const application = join(folder, "application");
  await mkdir(join(application, "src"), { recursive: true });
  const metadata = JSON.parse(await readFile(join(example, "package.json")));
  metadata.dependencies["@hibana-example/node-compat"] =
    `file:${join(folder, packed.filename)}`;
  metadata.devDependencies["@hibana/cli"] = `file:${resolve("sdk")}`;
  await writeFile(join(application, "package.json"), JSON.stringify(metadata));
  await copyFile(
    join(example, "hibana.json"),
    join(application, "hibana.json"),
  );
  await copyFile(
    join(example, "src/index.ts"),
    join(application, "src/index.ts"),
  );
  await runCommand(
    "npm",
    ["install", "--ignore-scripts", "--offline", "--no-audit", "--no-fund"],
    { cwd: application },
  );
  await assert.rejects(
    access(
      join(application, "node_modules/@hibana-example/node-compat/build.mjs"),
    ),
  );
  await runCommand(process.execPath, [resolve("sdk/src/cli.mjs"), "build"], {
    cwd: application,
    timeoutMs: 180000,
  });
  const config = await loadConfig(join(application, "hibana.json"));
  const artifact = join(application, ".hibana/build/app.wasm");
  const accepted = await validate(artifact);
  assert.ok(accepted.Ok, JSON.stringify(accepted));
  assert.ok(
    accepted.Ok.approved_imports.every((name) => name.startsWith("wasi:")),
  );
  console.log(
    "PASS installed extension tarball builds through ordinary CLI without author sources or manual WIT",
  );

  let uploaded = false;
  await deploy(
    {
      async request(path, options) {
        if (path === "/components" && !options)
          return [{ name: config.name, id: "example-id" }];
        assert.equal(path, "/components/example-id/versions");
        assert.equal(options.method, "POST");
        assert.deepEqual([...options.body.keys()].sort(), [
          "activate",
          "ingress",
          "resource_limits",
          "secrets",
          "vars",
          "version",
          "wasm",
        ]);
        assert.deepEqual(
          Buffer.from(await options.body.get("wasm").arrayBuffer()),
          await readFile(artifact),
        );
        uploaded = true;
        return {};
      },
    },
    config,
    artifact,
    "1.0.0",
  );
  assert.equal(uploaded, true);
  console.log(
    "PASS deployment uploads one Component through the existing API without granting capabilities",
  );

  await writeFile(
    settings,
    JSON.stringify({ vars: {}, resources: config.resources }),
  );
  const listener = createServer();
  await new Promise((done, reject) => {
    listener.once("error", reject);
    listener.listen(0, "127.0.0.1", done);
  });
  const port = listener.address().port;
  await new Promise((done) => listener.close(done));
  child = spawn(
    process.execPath,
    [
      resolve("sdk/src/cli.mjs"),
      "dev",
      "--no-watch",
      "--config",
      config.path,
      "--runtime",
      worker,
      "--port",
      String(port),
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let spawnError;
  child.on("error", (error) => {
    spawnError = error;
  });
  for (const stream of [child.stdout, child.stderr])
    stream.on("data", (bytes) => {
      logs = (logs + bytes.toString()).slice(-32768);
    });
  let url;
  for (let n = 0; n < 1200; n++) {
    if (spawnError) throw spawnError;
    assert.equal(child.exitCode, null, logs);
    assert.equal(child.signalCode, null, logs);
    url = logs.match(/Hibana \(Wasmtime\): (http:\/\/127\.0\.0\.1:\d+)/)?.[1];
    if (url) break;
    await sleep(100);
  }
  assert.ok(url, "runtime startup deadline: " + logs);
  for (const text of ["abc", "", "日本語 🔥", "abc"]) {
    const response = await fetch(`${url}/?` + new URLSearchParams({ text }), {
      signal: AbortSignal.timeout(30000),
    });
    assert.equal(response.status, 200, logs);
    assert.deepEqual(await response.json(), {
      sha256: createHash("sha256").update(text).digest("hex"),
      base64: Buffer.from(text).toString("base64"),
    });
  }
  await stop();
  console.log(
    "PASS ordinary hibana dev runs packaged Buffer + Rust SHA-256 on Wasmtime and shuts down",
  );
  await assert.rejects(fetch(`${url}/`, { signal: AbortSignal.timeout(2000) }));

  // An unresolved custom import cannot cause the server to load a host plugin.
  const unplugged = join(folder, "unplugged.wasm");
  const plan = await resolveExtensions(config);
  const wit = await prepareExtensionWit(plan, join(folder, "unplugged-inputs"));
  await compileJavaScript(config, unplugged, { ...plan, wit });
  const rejected = await validate(unplugged);
  assert.match(
    rejected.Rejected?.message || "",
    /unapproved host import 'example:crypto\/hash@1.0.0'/,
  );
  await assert.rejects(
    runCommand(worker, workerArgs(unplugged), { timeoutMs: 120000 }),
    /Command failed \(1\)/,
  );
  console.log(
    "PASS composed upload passes CP validation; missing user component is rejected by CP and Worker",
  );

  // A real WAC type/link failure must not replace the previous usable build.
  const previous = join(folder, ".hibana/build/app.wasm");
  await mkdir(join(folder, ".hibana/build"), { recursive: true });
  await copyFile(artifact, previous);
  const localExtension = join(folder, "extension");
  await mkdir(localExtension);
  await copyFile(artifact, join(localExtension, "wrong.wasm"));
  await writeFile(
    join(localExtension, "hibana.extension.json"),
    JSON.stringify({
      schemaVersion: 1,
      runtime: HTTP_CONTRACT,
      components: ["./wrong.wasm"],
    }),
  );
  await assert.rejects(
    build({ root: folder, component: unplugged, extensions: ["./extension"] }),
    /Could not compose/,
  );
  assert.deepEqual(await readFile(previous), await readFile(artifact));
  console.log(
    "PASS failed composition preserves the previous complete artifact",
  );
} finally {
  await stop();
  await rm(folder, { recursive: true, force: true });
}
