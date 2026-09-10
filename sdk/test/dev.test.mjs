import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { packageInfo, releaseBase } from "../src/package.mjs";

const cli = process.env.HIBANA_TEST_CLI || fileURLToPath(new URL("../src/cli.mjs", import.meta.url));
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "hibana-auto-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const version = (await packageInfo()).version;
  const target = `${process.platform}-${process.arch}`;
  const home = join(root, "runtimes"), bin = join(root, "bin");
  const managed = join(home, version, target, "hibana-worker");
  const calls = join(root, "downloads.jsonl"), preload = join(root, "fetch.mjs");
  const runtime = label => `#!${process.execPath}\nconsole.log("Runtime fixture: ${label}"); setInterval(() => {}, 1000);\n`;
  const bytes = runtime("downloaded");
  const hash = createHash("sha256").update(bytes).digest("hex");
  const asset = `hibana-worker-${version}-${target}`;
  await mkdir(bin);
  await writeFile(calls, "");
  await writeFile(join(root, "hibana.json"), JSON.stringify({ name: "hello", component: "app.wasm" }));
  await writeFile(join(root, "app.wasm"), Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]));
  // Stub only the child CLI's HTTP responses; exercise its real argument parsing,
  // runtime discovery, checksum validation, installation, build and process startup.
  await writeFile(preload, `
    import { appendFile } from "node:fs/promises";
    globalThis.fetch = async url => {
      await appendFile(${JSON.stringify(calls)}, JSON.stringify(String(url)) + "\\n");
      const mode = process.env.HIBANA_TEST_DOWNLOAD;
      if (mode === "network") throw new Error("fixture network unavailable");
      if (mode === "http") return new Response("missing", { status: 404 });
      if (String(url) === ${JSON.stringify(releaseBase(version) + "SHA256SUMS")})
        return new Response(${JSON.stringify(`${hash}  ${asset}\n`)});
      if (String(url) === ${JSON.stringify(releaseBase(version) + asset)})
        return new Response(mode === "checksum" ? "corrupt" : ${JSON.stringify(bytes)});
      throw new Error("Unexpected download: " + url);
    };
  `);
  async function writeRuntime(path, label) {
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, runtime(label), { mode: 0o700 });
    return path;
  }
  function invoke(args = ["dev", "--no-watch"], overrides = {}) {
    return new Promise((resolve, reject) => {
      const child = spawn(process.execPath, ["--import", preload, cli, ...args], {
        cwd: root, stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, PATH: bin, NODE_OPTIONS: "", HIBANA_RUNTIME_HOME: home,
          HIBANA_RUNTIME_BIN: "", HIBANA_TEST_DOWNLOAD: "", ...overrides },
      });
      let output = "", stopping = false;
      const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
      child.once("error", error => { clearTimeout(timer); reject(error); });
      child.once("close", (code, signal) => { clearTimeout(timer); resolve({ code, signal, output }); });
      child.stderr.on("data", b => output += b);
      child.stdout.on("data", b => {
        output += b;
        if (!stopping && output.includes("Runtime fixture:")) { stopping = true; child.kill("SIGINT"); }
      });
    });
  }
  const downloads = async () => (await readFile(calls, "utf8")).split("\n").filter(Boolean).map(line => JSON.parse(line));
  return { root, home, bin, managed, version, asset, bytes, invoke, writeRuntime, downloads };
}

test("dev installs the matching runtime on first use and reuses it on subsequent starts", async t => {
  const f = await fixture(t);
  const old = await f.writeRuntime(join(f.home, "0.0.0", `${process.platform}-${process.arch}`, "hibana-worker"), "old");
  const first = await f.invoke();
  assert.equal(first.code, 0, first.output);
  assert.match(first.output, /Downloading/);
  assert.match(first.output, /Runtime fixture: downloaded/);
  assert.deepEqual(await f.downloads(), [releaseBase(f.version) + "SHA256SUMS", releaseBase(f.version) + f.asset]);
  assert.equal(await readFile(f.managed, "utf8"), f.bytes);
  assert.equal((await stat(f.managed)).mode & 0o777, 0o700);
  assert.match(await readFile(old, "utf8"), /Runtime fixture: old/);
  const cached = await f.invoke();
  assert.equal(cached.code, 0, cached.output);
  assert.match(cached.output, /Runtime fixture: downloaded/);
  assert.doesNotMatch(cached.output, /Downloading/);
  assert.equal((await f.downloads()).length, 2);
});

test("dev preserves explicit, environment, managed and PATH runtime precedence without downloading", async t => {
  const f = await fixture(t);
  await f.writeRuntime(join(f.bin, "hibana-worker"), "path");
  let result = await f.invoke();
  assert.equal(result.code, 0, result.output); assert.match(result.output, /Runtime fixture: path/);
  await f.writeRuntime(f.managed, "managed");
  result = await f.invoke();
  assert.equal(result.code, 0, result.output); assert.match(result.output, /Runtime fixture: managed/);
  const env = { HIBANA_RUNTIME_BIN: await f.writeRuntime(join(f.root, "env-worker"), "environment") };
  result = await f.invoke(["dev", "--no-watch"], env);
  assert.equal(result.code, 0, result.output); assert.match(result.output, /Runtime fixture: environment/);
  const explicit = await f.writeRuntime(join(f.root, "explicit-worker"), "explicit");
  result = await f.invoke(["dev", "--no-watch", "--runtime", explicit], env);
  assert.equal(result.code, 0, result.output); assert.match(result.output, /Runtime fixture: explicit/);
  result = await f.invoke(["dev", "--no-watch", "--runtime", join(f.root, "missing-worker")], env);
  assert.notEqual(result.code, 0, result.output);
  assert.deepEqual(await f.downloads(), []);
});

test("a runtime installed from an offline file works with ordinary dev and no downloads", async t => {
  const f = await fixture(t);
  const source = await f.writeRuntime(join(f.root, "offline-worker"), "offline");
  const checksum = createHash("sha256").update(await readFile(source)).digest("hex");
  const installed = await f.invoke(["runtime", "install", "--from", source, "--sha256", checksum]);
  assert.equal(installed.code, 0, installed.output);
  const started = await f.invoke();
  assert.equal(started.code, 0, started.output);
  assert.match(started.output, /Runtime fixture: offline/);
  assert.deepEqual(await f.downloads(), []);
});

test("failed automatic downloads do not launch or cache a runtime, and a later dev can retry", async t => {
  const f = await fixture(t);
  for (const mode of ["network", "http", "checksum"]) {
    const result = await f.invoke(["dev", "--no-watch"], { HIBANA_TEST_DOWNLOAD: mode });
    assert.equal(result.code, 1, result.output);
    assert.match(result.output, /Could not prepare the local runtime/);
    assert.match(result.output, /run hibana dev again/);
    assert.doesNotMatch(result.output, /Runtime fixture:/);
    await assert.rejects(stat(f.managed), { code: "ENOENT" });
    await assert.rejects(stat(join(f.root, ".hibana")), { code: "ENOENT" });
  }
  const retry = await f.invoke();
  assert.equal(retry.code, 0, retry.output);
  assert.match(retry.output, /Runtime fixture: downloaded/);
});

test("building a Component and reading dev help do not install a runtime", async t => {
  const f = await fixture(t);
  const built = await f.invoke(["build"]);
  assert.equal(built.code, 0, built.output);
  const help = await f.invoke(["dev", "--help"]);
  assert.equal(help.code, 0, help.output);
  assert.doesNotMatch(help.output, /--no-runtime-install/);
  assert.doesNotMatch(help.output, /HIBANA_NO_RUNTIME_INSTALL/);
  assert.deepEqual(await f.downloads(), []);
});
