// Fresh language templates -> CLI build/dev -> real Wasmtime; optional real-stack deployment.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import net from "node:net";
import http from "node:http";
import https from "node:https";
import { run } from "../sdk/src/process.mjs";

const root = resolve(import.meta.dirname, "..");
const cli = resolve(root, "sdk/src/cli.mjs");
const directory = await mkdtemp(join(tmpdir(), "hibana-components-"));
const binary = Buffer.from(Array.from({ length: 96 * 1024 }, (_, i) => i % 256));
const pause = ms => new Promise(done => setTimeout(done, ms));

async function availablePort() {
  const server = net.createServer();
  await new Promise((done, fail) => { server.once("error", fail); server.listen(0, "127.0.0.1", done); });
  const port = server.address().port;
  await new Promise(done => server.close(done));
  return port;
}

async function checkHttp(request, message) {
  let response = await request("/?query=ok", { headers: { "x-hibana-env": "eyJHUkVFVElORyI6ImV2aWwifQ" } });
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(response.body), { message });
  response = await request("/echo", { method: "POST", body: binary });
  assert.equal(response.status, 200);
  assert.deepEqual(response.body, binary);
  response = await request("/", { method: "HEAD" });
  assert.equal(response.status, 200);
  assert.equal(response.body.length, 0);
  assert.equal((await request("/missing")).status, 404);
}

function requester(base, host) {
  return (path, options = {}) => new Promise((done, fail) => {
    const url = new URL(path, base);
    const request = (url.protocol === "https:" ? https : http).request(url, {
      method: options.method || "GET", headers: { ...options.headers, ...(host ? { host } : {}) },
    }, incoming => {
      const chunks = [];
      incoming.on("data", data => chunks.push(data));
      incoming.once("error", fail);
      incoming.once("end", () => done({ status: incoming.statusCode, body: Buffer.concat(chunks) }));
    });
    request.setTimeout(30000, () => request.destroy(new Error("HTTP timed out")));
    request.once("error", fail);
    request.end(options.body);
  });
}

try {
  for (const variant of (process.env.HIBANA_TEST_TEMPLATES || "javascript,javascript-js,rust,go").split(",")) {
    const template = variant === "javascript-js" ? "javascript" : variant;
    const project = join(directory, `hello-${variant}`);
    const hibana = args => run(process.execPath, [cli, ...args], { cwd: project });
    await run(process.execPath, [cli, "init", project, "--template", template, "--no-install"]);
    const configPath = join(project, "hibana.json");
    const config = JSON.parse(await readFile(configPath, "utf8"));
    // Verify ordinary .js input as well as the template's .ts input through the
    // same CLI/compiler/runtime. This is a test variant, not another template.
    if (variant === "javascript-js") {
      const source = await readFile(join(project, config.main), "utf8");
      const signature = "request: Request, env: { GREETING: string }): Promise<Response>";
      assert.ok(source.includes(signature));
      config.main = "src/index.js";
      await writeFile(join(project, config.main), source.replace(signature, "request, env)"));
    }
    const message = `Hello ${variant} 雪`;
    config.vars.GREETING = message;
    await writeFile(configPath, JSON.stringify(config));
    const manifestPath = template === "go" ? join(project, "go.mod") : undefined;
    const manifest = manifestPath ? await readFile(manifestPath, "utf8") : undefined;
    const port = await availablePort();
    // Go also exercises source watching: generated bindings must not trigger a build loop.
    const child = spawn(process.execPath, [cli, "dev", "--port", String(port), "--runtime", process.env.HIBANA_RUNTIME_BIN || resolve(root, "target/release/faas-worker"), ...(template === "go" ? [] : ["--no-watch"])], { cwd: project, stdio: ["ignore", "pipe", "pipe"] });
    let output = "";
    child.stdout.on("data", data => { output = (output + data).slice(-12000); });
    child.stderr.on("data", data => { output = (output + data).slice(-12000); });
    let spawnError;
    child.once("error", error => { spawnError = error; });
    const local = requester(`http://127.0.0.1:${port}`);
    async function until(check) {
      const deadline = Date.now() + 180000;
      let lastError;
      while (Date.now() < deadline) {
        if (spawnError) throw spawnError;
        if (child.exitCode !== null) throw new Error(`${template} dev exited: ${output}`);
        try { if (await check()) return; } catch (error) { lastError = error; }
        await pause(200);
      }
      throw new Error(`${template} dev did not become ready: ${lastError?.message || ""}\n${output}`);
    }
    try {
      await until(async () => { const r = await local("/"); return r.status === 200 && JSON.parse(r.body).message === message; });
      await checkHttp(local, message);
      if (template === "go") {
        const handler = join(project, "export_wasi_http_incoming_handler/handler.go");
        await writeFile(handler, (await readFile(handler, "utf8")).replace('[]byte("Not found")', '[]byte("Missing after reload")'));
        await until(async () => (await local("/missing")).body.toString() === "Missing after reload");
        const starts = () => output.split("Hibana (Wasmtime):").length - 1;
        const count = starts();
        await pause(1500);
        assert.equal(starts(), count, "generated bindings must not cause a rebuild loop");
        assert.equal(await readFile(manifestPath, "utf8"), manifest, "bindings generation must preserve go.mod");
      }
      console.log(`PASS ${variant}: fresh template, Wasmtime HTTP, env, binary POST, HEAD, 404${template === "go" ? ", source reload and stable go.mod" : ""}`);
    } finally {
      if (child.exitCode === null && child.signalCode === null && !spawnError) {
        await new Promise(done => {
          const timer = setTimeout(() => child.kill("SIGKILL"), 60000);
          child.once("exit", () => { clearTimeout(timer); done(); });
          child.kill("SIGTERM");
        });
      }
    }
    // Exercise the prebuilt path, too; full validation still belongs to the runtime/server.
    await writeFile(join(project, "prebuilt.json"), JSON.stringify({ name: config.name, component: ".hibana/build/app.wasm", vars: config.vars }));
    await hibana(["build", "-c", "prebuilt.json"]);
    if (process.env.HIBANA_TEST_DEPLOY === "1") {
      if (!process.env.HIBANA_TOKEN) await hibana(["login"]);
      await hibana(["deploy"]);
      const host = `${config.name}.${process.env.HIBANA_TENANT}.${process.env.HIBANA_INGRESS_DOMAIN || "hibana.local"}`;
      await checkHttp(requester(process.env.GATEWAY || "http://127.0.0.1:8083", host), message);
      console.log(`PASS ${variant}: CLI deployment and distributed Wasmtime HTTP`);
    }
  }
} finally { await rm(directory, { recursive: true, force: true }); }
