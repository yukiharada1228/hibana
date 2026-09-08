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
    for (const invalid of [{ component: "a.wasm" }, { name: "hello-" }, { vars: null }, { vars: { lowercase: "bad" } }, { vars: { VALUE: "x".repeat(4097) } }, { secrets: null }, { secrets: ["A", "A"] }, { secrets: ["lowercase"] }, { secrets: ["A"], vars: { A: "collision" } }, { limits: [] }, { limits: { timeout_ms: 30001 } }, { kv_namespaces: [] }, { http: false }, { http: true }]) {
      await writeFile(path, JSON.stringify({ ...valid, ...invalid }));
      await assert.rejects(loadConfig(path));
    }
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("deploy publishes code, vars and selected Secrets with one request and no admin operations", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-deploy-"));
  try {
    const artifact = join(dir, "app.wasm"); await writeFile(artifact, "fixture");
    const calls = [];
    const api = { async request(path, options = {}) {
      calls.push({ path, ...options });
      if (path === "/components") return [{ name: "hello", component_id: "cmp" }];
      return {};
    } };
    const config = { name: "hello", vars: { GREETING: "hello" }, secrets: ["SELECTED"], resources: {} };
    await deploy(api, config, artifact, "1.0.0");
    assert.deepEqual(calls.map(c => c.path), ["/components", "/components/cmp/versions"]);
    const form = calls[1].body;
    assert.equal(form.get("activate"), "true");
    assert.equal(form.get("ingress"), "true");
    assert.deepEqual(JSON.parse(form.get("vars")), config.vars);
    assert.deepEqual(JSON.parse(form.get("secrets")), ["SELECTED"]);
    const failed = [];
    await assert.rejects(deploy({ async request(path, options) { failed.push(path); if (path.endsWith("/versions")) throw Error("denied"); return api.request(path, options); } }, config, artifact, "1.0.1"), /denied/);
    assert.deepEqual(failed, ["/components", "/components/cmp/versions"]);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("init scaffolds ordinary Hono and refuses to overwrite files", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-init-"));
  try {
    const cli = new URL("../src/cli.mjs", import.meta.url);
    execFileSync(process.execPath, [cli.pathname, "init", dir, "--template", "hono", "--no-install"]);
    assert.match(await readFile(join(dir, "src/index.ts"), "utf8"), /export default app;?/);
    assert.equal((await loadConfig(join(dir, "hibana.json"))).main, "src/index.ts");
    const pkg = JSON.parse(await readFile(join(dir, "package.json"), "utf8"));
    assert.deepEqual(Object.keys(pkg.dependencies), ["hono"]);
    const metadata = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
    assert.equal(metadata.private, true, "The CLI must not be published to npm");
    assert.equal(pkg.devDependencies["@hibana/cli"], `https://github.com/yukiharada1228/hibana/releases/download/v${metadata.version}/hibana-cli-${metadata.version}.tgz`);
    assert.throws(() => execFileSync(process.execPath, [cli.pathname, "init", dir, "--no-install"], { stdio: "pipe" }));
  } finally { await rm(dir, { recursive: true, force: true }); }
});
