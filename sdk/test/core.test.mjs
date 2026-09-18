import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { loadConfig } from "../src/config.mjs";
import { deploy } from "../src/api.mjs";
import { validateVersionName } from "../src/version-name.mjs";

test("deploy validates version names before building or contacting the API", async () => {
  for (const version of ["1", "v1.2.3-rc.1+build_7", "a".repeat(128)])
    assert.equal(validateVersionName(version), version);
  const api = { async request() { assert.fail("invalid names must not contact the API"); } };
  for (const version of [undefined, null, 1, {}, "", ".", "..", "../v1", "a/b", "a\\b", "%2e", "a?b", "a#b", "a b", " a", "a\n", "a\r", "版1", "a".repeat(129)])
    await assert.rejects(deploy(api, {}, "missing.wasm", version), /Version must be/);
  const dir = await mkdtemp(join(tmpdir(), "hibana-version-"));
  try {
    // A build would fail because the configured component is absent.
    await writeFile(join(dir,"hibana.json"), JSON.stringify({name:"version-test",component:"missing.wasm"}));
    const cli = new URL("../src/cli.mjs", import.meta.url);
    for (const version of ["..", "", "a\n"]) {
      assert.throws(() => execFileSync(process.execPath,[cli.pathname,"deploy","--url","http://localhost:1","--version",version], {cwd:dir,stdio:"pipe"}), error => {
        assert.match(error.stderr.toString(), version === "" ? /requires a non-empty VERSION/ : /Version must be/);
        return true;
      });
    }
  } finally { await rm(dir, {recursive:true,force:true}); }
});

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
    assert.equal(JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8")).private, true, "The CLI must not be published to npm");
    assert.equal(pkg.devDependencies?.["@hibana/cli"], undefined, "init uses the installed CLI, including unpublished versions");
    assert.deepEqual(pkg.scripts, { dev: "hibana dev", build: "hibana build", deploy: "hibana deploy" });
    assert.throws(() => execFileSync(process.execPath, [cli.pathname, "init", dir, "--no-install"], { stdio: "pipe" }));
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("init can explicitly pin a project-local CLI without changing the default", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-pin-"));
  try {
    const cli = new URL("../src/cli.mjs", import.meta.url);
    const supplied = join(dir, "private-cli.tgz");
    const project = join(dir, "app");
    execFileSync(process.execPath, [cli.pathname, "init", project, "--cli-package", supplied, "--no-install"]);
    const pkg = JSON.parse(await readFile(join(project, "package.json"), "utf8"));
    assert.equal(pkg.devDependencies["@hibana/cli"], `file:${supplied}`);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

// These labels are accepted by the API; every local entry point must agree.
test("numeric DNS labels work through init, config, deploy and deletion", async () => {
  const dir = await mkdtemp(join(tmpdir(), "hibana-names-"));
  const cli = new URL("../src/cli.mjs", import.meta.url);
  try {
    for (const name of ["0", "2026-api", "9".repeat(63)]) {
      const root = join(dir, name);
      execFileSync(process.execPath, [cli.pathname, "init", root, "--no-install"]);
      const config = await loadConfig(join(root, "hibana.json"));
      assert.equal(config.name, name);
      assert.match(execFileSync(process.execPath, [cli.pathname, "delete", name, "--dry-run"], {cwd:dir, encoding:"utf8"}), /Would delete application/);
      assert.match(execFileSync(process.execPath, [cli.pathname, "delete", "--dry-run"], {cwd:root, encoding:"utf8"}), /Would delete application/);
      const artifact = join(root, "fixture.wasm");
      await writeFile(artifact, "fixture");
      const calls = [];
      await deploy({async request(path, options = {}) {
        calls.push({path, ...options});
        if (path === "/components" && !options.method) return [];
        if (path === "/components") return {component_id:"fixture"};
        return {};
      }}, config, artifact, "1");
      assert.equal(calls[1].body.name, name);
      assert.equal(calls[2].path, "/components/fixture/versions");
    }
    for (const name of [42, null, {}, "name\n", "name\r", " name", "name ", "a.b", "A", "-a", "a-", "a".repeat(64)]) {
      const path = join(dir, "hibana.json");
      await writeFile(path, JSON.stringify({name, main:"index.ts"}));
      await assert.rejects(loadConfig(path), /DNS label/);
      assert.throws(() => execFileSync(process.execPath, [cli.pathname, "delete", "--dry-run"], {cwd:dir, stdio:"pipe"}));
    }
  } finally { await rm(dir, {recursive:true, force:true}); }
});
