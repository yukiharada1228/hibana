import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { platformCommand } from "../src/platform.mjs";

const cli = fileURLToPath(new URL("../src/cli.mjs", import.meta.url));
function invoke(args, cwd, env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cli, ...args], { cwd, env: { ...process.env, HIBANA_TOKEN: "test-token", BOOTSTRAP_ADMIN_TOKEN: "", ...env }, stdio: ["ignore", "pipe", "pipe"] });
    let output = "";
    child.stdout.on("data", b => output += b);
    child.stderr.on("data", b => output += b);
    child.on("error", reject);
    child.on("exit", code => resolve({ code, output }));
  });
}

test("delete supports Wrangler name/config and offline dry-run without source files", async t => {
  const cwd = await mkdtemp(join(tmpdir(), "hibana-delete-"));
  t.after(() => rm(cwd, { recursive: true, force: true }));
  await writeFile(join(cwd, "hibana.json"), JSON.stringify({ name: "hello", main: "missing.ts" }));
  for (const args of [[], ["hello"], ["--name", "hello"], ["-c", "hibana.json"]]) {
    const r = await invoke(["delete", ...args, "--dry-run"], cwd, { HIBANA_URL: "http://127.0.0.1:1", HIBANA_TOKEN: "" });
    assert.equal(r.code, 0, r.output);
    assert.match(r.output, /Would delete application hello/);
  }
  for (const args of [["hello", "--name", "other"], ["hello", "--all"], ["--all-tenants"], ["../invalid"]]) {
    assert.notEqual((await invoke(["delete", ...args, "--dry-run"], cwd)).code, 0);
  }
});

test("delete confirms, scopes admin credentials, preserves conflicts and handles empty inventory", async t => {
  const cwd = await mkdtemp(join(tmpdir(), "hibana-delete-"));
  t.after(() => rm(cwd, { recursive: true, force: true }));
  let inventory = [{ name: "hello", component_id: "cmp-1", tenant_id: "ten-1", tenant_slug: "load-test" }];
  let status = 204;
  const calls = [];
  const server = createServer((req, res) => {
    calls.push({ path: req.url, method: req.method, token: req.headers.authorization });
    res.setHeader("Content-Type", "application/json");
    if (req.method === "GET") res.end(JSON.stringify(inventory));
    else { res.statusCode = status; res.end(status === 204 ? undefined : JSON.stringify({ error: { message: "active execution" } })); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const env = { HIBANA_URL: `http://127.0.0.1:${server.address().port}` };
  const unconfirmed = await invoke(["delete", "hello"], cwd, env);
  assert.notEqual(unconfirmed.code, 0);
  assert.match(unconfirmed.output, /--yes/);
  assert.equal(calls.filter(c => c.method === "DELETE").length, 0);
  const missingAdmin = await invoke(["delete", "--all", "--all-tenants", "--yes"], cwd, env);
  assert.notEqual(missingAdmin.code, 0);
  const adminEnv = { ...env, BOOTSTRAP_ADMIN_TOKEN: "admin-only" };
  assert.equal((await invoke(["delete", "--all", "--all-tenants", "--dry-run"], cwd, adminEnv)).code, 0);
  assert.equal(calls.filter(c => c.method === "DELETE").length, 0);
  const deleted = await invoke(["delete", "--all", "--all-tenants", "--yes"], cwd, adminEnv);
  assert.equal(deleted.code, 0, deleted.output);
  assert.deepEqual(calls.at(-1), { path: "/admin/tenants/ten-1/components/cmp-1", method: "DELETE", token: "Bearer admin-only" });
  status = 409;
  const conflict = await invoke(["delete", "hello", "--force"], cwd, adminEnv);
  assert.notEqual(conflict.code, 0);
  assert.match(conflict.output, /409/);
  assert.equal(calls.at(-1).token, "Bearer test-token");
  inventory = [];
  assert.equal((await invoke(["delete", "--all", "--yes"], cwd, env)).code, 0);
});

test("platform targets are explicit and validated before mutations", () => {
  assert.match(platformCommand(["install"], { source: "." }).command.join(" "), /install --cluster hibana$/);
  assert.throws(() => platformCommand(["install"], {}), /Select an existing cluster/);
  assert.throws(() => platformCommand(["stop"], { context: "production" }), /both/);
  assert.throws(() => platformCommand(["stop"], { kubeconfig: "config", context: "prod", cluster: "local" }), /cannot be combined/);
  assert.throws(() => platformCommand(["install"], { kubeconfig: "config", context: "prod" }), /overlay/);
  const result = platformCommand(["install"], { kubeconfig: "config", context: "prod", overlay: "overlay", image: "image@sha256:abc" });
  assert.ok(result.command.includes("prod"));
  assert.ok(!result.command.includes("--cluster"));
});

test("stopping dev during its first build does not leave a file watcher running", async t => {
  const cwd = await mkdtemp(join(tmpdir(), "hibana-stop-build-"));
  t.after(() => rm(cwd, { recursive: true, force: true }));
  const module = new URL("../src/dev.mjs", import.meta.url).href;
  const code = `import { dev } from ${JSON.stringify(module)}; await dev({root:process.cwd()}, {runtime:process.execPath}, async () => { console.log('building'); await new Promise(r=>setTimeout(r,300)); return 'unused.wasm'; });`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", code], { cwd, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  const exit = new Promise(resolve => child.once("exit", (code, signal) => resolve({ code, signal })));
  const timer = setTimeout(() => child.kill("SIGKILL"), 5000);
  t.after(() => { clearTimeout(timer); child.kill(); });
  child.stdout.on("data", b => { output += b; if (output.includes("building")) child.kill("SIGTERM"); });
  const result = await exit;
  assert.equal(result.code, 0);
  assert.equal(result.signal, null);
});

test("stopping dev during a rebuild waits for the old runtime and never spawns another", async t => {
  const cwd = await mkdtemp(join(tmpdir(), "hibana-stop-rebuild-"));
  const runtime = join(cwd, "runtime.mjs");
  await writeFile(runtime, `#!${process.execPath}\nconsole.log('RUNTIME_STARTED');setInterval(()=>{},1000);process.once('SIGTERM',()=>{console.log('RUNTIME_DRAINING');setTimeout(()=>process.exit(0),350)});`, {mode:0o700});
  await writeFile(join(cwd, "hibana.json"), JSON.stringify({name:"review",main:"main.js"}));
  await writeFile(join(cwd, "main.js"), "initial");
  const module = new URL("../src/dev.mjs", import.meta.url).href;
  const config = new URL("../src/config.mjs", import.meta.url).href;
  const code = `import {dev} from ${JSON.stringify(module)};import {loadConfig} from ${JSON.stringify(config)};await dev(await loadConfig(),{runtime:${JSON.stringify(runtime)}},async()=> 'unused.wasm');console.log('DEV_FINISHED');`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", code], {cwd, detached: true, stdio:["ignore","pipe","pipe"]});
  let output = "", changed = false, stopped = false;
  const timer = setTimeout(()=>{try{process.kill(-child.pid,"SIGKILL")}catch{}},5000);
  t.after(async()=>{clearTimeout(timer);try{process.kill(-child.pid,"SIGKILL")}catch{} await rm(cwd,{recursive:true,force:true});});
  child.stdout.on("data", b=>{
    output += b;
    if(!changed && output.includes("Watching") && output.includes("RUNTIME_STARTED")) {
      changed = true; writeFile(join(cwd,"main.js"),"updated").catch(()=>child.kill("SIGKILL"));
    }
    if(!stopped && output.includes("RUNTIME_DRAINING")) { stopped=true; child.kill("SIGTERM"); }
  });
  child.stderr.on("data",b=>output+=b);
  const result = await new Promise(done=>child.once("exit",(code,signal)=>done({code,signal})));
  assert.deepEqual(result,{code:0,signal:null},output);
  assert.ok(stopped,output);
  assert.match(output,/DEV_FINISHED/);
  assert.equal(output.match(/RUNTIME_STARTED/g)?.length,1,output);
});
