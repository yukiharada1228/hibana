import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { loadConfig } from "../src/config.mjs";
import { deploy } from "../src/api.mjs";

test("config rejects ambiguous workloads and limits rejected by the server", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-config-"));
  try {
    const path = join(dir, "hibana.json");
    const valid = { name: "hello", main: "src/index.ts" };
    await writeFile(path, JSON.stringify(valid));
    assert.equal((await loadConfig(path)).resources.max_memory_bytes, 256 * 1024 * 1024);
    for (const invalid of [{ component: "a.wasm" }, { name: "hello-" }, { vars: null }, { limits: [] }, { limits: { timeout_ms: 30001 } }, { kv_namespaces: [] }, { http: false }, { http: true }]) {
      await writeFile(path, JSON.stringify({ ...valid, ...invalid }));
      await assert.rejects(loadConfig(path));
    }
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("deploy activates only after environment grants succeed", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-deploy-"));
  try {
    const artifact = join(dir, "app.wasm"); await writeFile(artifact, "fixture");
    const calls = [];
    const api = { async request(path, options = {}) {
      calls.push({ path, ...options });
      if (path === "/components") return [{ name: "hello", component_id: "cmp" }];
      if (path.endsWith("/secrets/keys")) return { secrets: [{ name: "SECRET" }] };
      return {};
    } };
    const config = { name: "hello", vars: { GREETING: "hello" }, resources: {} };
    await deploy(api, config, artifact, "1.0.0");
    assert.equal(calls.find(c => c.path.endsWith("/versions")).body.get("activate"), "false");
    assert.deepEqual(calls.find(c => c.path.endsWith("/capabilities")).body.env, ["GREETING", "SECRET"]);
    assert.ok(calls.findIndex(c => c.path.endsWith("/capabilities")) < calls.findIndex(c => c.path.endsWith("/active-version")));
    const failed = [];
    await assert.rejects(deploy({ async request(path, options) { failed.push(path); if (path.endsWith("/capabilities")) throw Error("denied"); return api.request(path, options); } }, config, artifact, "1.0.1"), /denied/);
    assert.ok(!failed.some(p => p.endsWith("/active-version")));
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("init scaffolds ordinary Hono and refuses to overwrite files", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-init-"));
  try {
    const cli = new URL("../src/cli.mjs", import.meta.url);
    execFileSync(process.execPath, [cli.pathname, "init", dir, "--template", "hono", "--no-install"]);
    assert.match(await readFile(join(dir, "src/index.ts"), "utf8"), /export default app;/);
    assert.equal((await loadConfig(join(dir, "hibana.json"))).main, "src/index.ts");
    const pkg = JSON.parse(await readFile(join(dir, "package.json"), "utf8"));
    assert.deepEqual(Object.keys(pkg.dependencies), ["hono"]);
    assert.throws(() => execFileSync(process.execPath, [cli.pathname, "init", dir, "--no-install"], { stdio: "pipe" }));
  } finally { await rm(dir, { recursive: true, force: true }); }
});
