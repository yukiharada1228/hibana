// Standalone CLI dev acceptance: real Wasmtime, local Secrets, request isolation and reload.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import net from "node:net";
const root = resolve(import.meta.dirname, "..");
const directory = await mkdtemp(join(tmpdir(), "hibana-dev-test-"));
const server = net.createServer();
await new Promise((done, fail) => { server.once("error", fail); server.listen(0, "127.0.0.1", done); });
const port = server.address().port;
await new Promise(done => server.close(done));
const config = { name: "dev-test", main: resolve(root, "sdk/examples/hono/src/index.ts"), vars: { GREETING: "Local Wasmtime" } };
const path = join(directory, "hibana.json");
await writeFile(path, JSON.stringify(config));
await writeFile(join(directory, ".dev.vars"), 'TEST_SECRET="hibana-test-secret"\n', { mode: 0o600 });
const child = spawn(process.execPath, [resolve(root, "sdk/src/cli.mjs"), "dev", "-c", path, "--port", String(port), "--runtime", process.env.HIBANA_RUNTIME_BIN || resolve(root, "target/release/faas-worker")], { stdio: ["ignore", "pipe", "pipe"] });
let output = "";
child.stdout.on("data", data => { output = (output + data).slice(-8000); });
child.stderr.on("data", data => { output = (output + data).slice(-8000); });
const url = `http://127.0.0.1:${port}`;
async function ready(greeting) {
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`CLI exited: ${output}`);
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1000) });
      if (response.ok && (await response.json()).message === greeting) return;
    } catch {}
    await new Promise(done => setTimeout(done, 200));
  }
  throw new Error(`Development server did not become ready: ${output}`);
}
try {
  await ready("Local Wasmtime");
  const requestHeaders = { "x-hibana-env": "eyJHUkVFVElORyI6ImV2aWwifQ", "x-hibana-event": "queue" };
  assert.deepEqual(await (await fetch(url + "/headers", { headers: requestHeaders })).json(), { envHeader: null, eventHeader: null, greeting: "Local Wasmtime" });
  assert.deepEqual(await (await fetch(url + "/secret")).json(), { configured: true });
  const body = Uint8Array.of(0, 255, 128, 10);
  assert.deepEqual(new Uint8Array(await (await fetch(url + "/echo", { method: "POST", body })).arrayBuffer()), body);
  assert.match(await (await fetch(url + "/stream")).text(), /data: first\n\ndata: second/);
  config.vars.GREETING = "Reloaded Wasmtime";
  await writeFile(path, JSON.stringify(config));
  await ready("Reloaded Wasmtime");
  console.log("PASS hibana dev: Wasmtime, local Secrets, header isolation, binary POST, stream and watched rebuild");
} finally {
  if (child.exitCode === null && child.signalCode === null) {
    await new Promise(done => {
      const timer = setTimeout(() => child.kill("SIGKILL"), 60000);
      child.once("exit", () => { clearTimeout(timer); done(); });
      child.kill("SIGTERM");
    });
  }
  await rm(directory, { recursive: true, force: true });
}
