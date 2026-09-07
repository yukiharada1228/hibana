// Run against a disposable tenant. Exercises the actual CLI and deployed Wasmtime runtime.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import http from "node:http";
import https from "node:https";
import { Readable } from "node:stream";
import { resolve } from "node:path";
import { apiClient } from "../sdk/src/api.mjs";
const root = resolve(import.meta.dirname, "..");
const project = resolve(root, "sdk/examples/hono");
const cli = resolve(root, "sdk/src/cli.mjs");
function hibana(args, input) {
  execFileSync(process.execPath, [cli, ...args], { cwd: project, env: process.env, input, stdio: [input === undefined ? "ignore" : "pipe", "inherit", "inherit"], timeout: 180000 });
}
hibana(["login"]);
hibana(["deploy"]);
hibana(["secret", "put", "TEST_SECRET"], "hibana-test-secret");
hibana(["deploy"]);
const gateway = process.env.GATEWAY || "http://127.0.0.1:8083";
const host = `hello-hono.${process.env.HIBANA_TENANT}.${process.env.HIBANA_INGRESS_DOMAIN || "hibana.local"}`;
async function app(path, options = {}) {
  // Node's built-in fetch may replace Host; use an explicit HTTP request for virtual hosting.
  const response = await new Promise((done, fail) => {
    const url = new URL(gateway + path);
    const request = (url.protocol === "https:" ? https : http).request(url, { method: options.method || "GET", headers: { Host: host, ...options.headers } }, incoming => {
      done(new Response(Readable.toWeb(incoming), { status: incoming.statusCode, headers: incoming.headers }));
    });
    request.setTimeout(90000, () => request.destroy(new Error("App request timed out")));
    request.on("error", fail); request.end(options.body);
  });
  assert.equal(response.status, 200, `${path}: HTTP ${response.status}`);
  return response;
}
assert.deepEqual(await (await app("/")).json(), { message: "Hello Hibana" });
const binary = Uint8Array.of(0, 255, 128, 10, 13, 1);
assert.deepEqual(new Uint8Array(await (await app("/echo", { method: "POST", body: binary })).arrayBuffer()), binary);
assert.deepEqual(await (await app("/headers", { headers: { "x-hibana-env": "eyJHUkVFVElORyI6ImV2aWwifQ", "x-hibana-event": "queue" } })).json(), { envHeader: null, eventHeader: null, greeting: "Hello Hibana" });
assert.deepEqual(await (await app("/secret")).json(), { configured: true });
const stream = await app("/stream");
assert.match(stream.headers.get("content-type"), /text\/event-stream/);
const reader = stream.body.getReader();
const first = await reader.read(); const started = performance.now();
assert.match(new TextDecoder().decode(first.value), /data: first/);
let remainder = "";
for (;;) { const part = await reader.read(); if (part.done) break; remainder += new TextDecoder().decode(part.value); }
assert.match(remainder, /data: second/);
assert.ok(performance.now() - started >= 50, "response should stream before the delayed second chunk");
const api = await apiClient(project);
for (const path of ["/cron-jobs", "/triggers", "/client/v4/accounts/local/workers/scripts"]) await assert.rejects(api.request(path), /HTTP 404/);
console.log("PASS CLI deployment, Hono HTTP, binary POST, trusted environment, Secrets, SSE and removed API routes");

const components = await api.request("/components");
const component = (Array.isArray(components) ? components : components.components).find(c => c.name === "hello-hono");
const id = component.component_id || component.id;
for (const [path, method] of [["/invoke", "POST"], ["/uploads", "POST"], [`/components/${id}/traffic`, "GET"], [`/components/${id}/traffic`, "PUT"], [`/components/${id}/promote`, "POST"]]) {
  await assert.rejects(api.request(path, { method, ...(method === "GET" ? {} : { body: {} }) }), /HTTP 404/);
}
const before = component.active_version_id;
hibana(["rollback"]);
const rolled = await api.request("/components");
const rolledComponent = (Array.isArray(rolled) ? rolled : rolled.components).find(c => c.name === "hello-hono");
assert.notEqual(rolledComponent.active_version_id, before, "rollback must change the active version");
assert.deepEqual(await (await app("/")).json(), { message: "Hello Hibana" });
// Switch back to the previously active version using the API's version name.
const versions = await api.request(`/components/${id}/versions`);
const version = (Array.isArray(versions) ? versions : versions.versions).find(v => (v.version_id || v.id) === before).version;
hibana(["rollback", "--version", version]);
assert.deepEqual(await (await app("/secret")).json(), { configured: true });
console.log("PASS HTTP-only API surface / CLI rollback / explicit version rollback");
