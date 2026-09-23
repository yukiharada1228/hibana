import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const cli = process.env.HIBANA_TEST_CLI || fileURLToPath(new URL("../src/cli.mjs", import.meta.url));
const event = (id, overrides = {}) => ({
  execution_id: id, version_id: "ver_1", status: "succeeded", http_status: 404,
  created_at: "2026-09-23T14:52:41.457876Z", wall_time_ms: 12, error: null,
  logs: { stdout: "", stderr: "", truncated: false }, ...overrides,
});
const page = (items = [], cursor = "1-0", extra = {}) => ({ items, cursor, has_more: false, lagged: false, ...extra });

async function fixture(t, handler = () => page()) {
  const root = await mkdtemp(join(tmpdir(), "hibana-tail-")), calls = [];
  const server = createServer(async (req, res) => {
    calls.push(req.url);
    const url = new URL(req.url, "http://fixture.invalid");
    res.setHeader("Content-Type", "application/json");
    if (url.pathname === "/components") return res.end(JSON.stringify([{name: "hello", component_id: "cmp_1"}]));
    assert.equal(url.pathname, "/components/cmp_1/tail");
    assert.equal(req.headers.authorization, "Bearer fixture-tail");
    const result = await handler(url, res);
    if (result) res.end(JSON.stringify(result));
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await rm(root, {recursive: true, force: true});
  });
  function run(args, observe = () => {}) {
    const child = spawn(process.execPath, [cli, "tail", ...args], {
      cwd: root, stdio: ["ignore", "pipe", "pipe"],
      env: {...process.env, NODE_OPTIONS: "", TZ: "Asia/Tokyo", HIBANA_CONFIG_HOME: join(root, "profiles"),
        HIBANA_PROFILE: "", HIBANA_URL: `http://127.0.0.1:${server.address().port}`, HIBANA_TOKEN: "fixture-tail"},
    });
    let stdout = "", stderr = "";
    const finished = new Promise((resolve, reject) => {
      const timer = setTimeout(() => { child.kill("SIGKILL"); reject(new Error(`tail deadline: ${stdout}\n${stderr}`)); }, 12000);
      child.stdout.on("data", data => { stdout += data; observe({child, stdout, stderr}); });
      child.stderr.on("data", data => { stderr += data; observe({child, stdout, stderr}); });
      child.once("error", error => {clearTimeout(timer); reject(error);});
      child.once("close", (code, signal) => {clearTimeout(timer); resolve({code, signal, stdout, stderr});});
    });
    t.after(() => { if (child.exitCode === null) child.kill("SIGKILL"); });
    return {child, finished};
  }
  return {root, calls, run};
}

test("tail validates Wrangler-style options before network access", async t => {
  const f = await fixture(t);
  for (const args of [["--format", "xml"], ["--status", "404"], ["--search", "x".repeat(1025)], ["--version-id", "x".repeat(129)], ["--follow"], ["--since", "10m"]]) {
    const result = await f.run(["hello", ...args]).finished;
    assert.equal(result.code, 1);
  }
  assert.deepEqual(f.calls, []);
});

test("tail resumes its cursor, drains pages, deduplicates and keeps JSON stdout clean", async t => {
  let n = 0;
  const a = event("exec_1"), b = event("exec_2", {status: "timeout", error: {message: "deadline"}});
  const f = await fixture(t, (url, res) => {
    assert.equal(url.searchParams.get("status"), "error");
    assert.equal(url.searchParams.get("search"), "雪 & timeout");
    assert.equal(url.searchParams.get("version_id"), "ver_1");
    switch (n++) {
      case 0: assert.equal(url.searchParams.has("cursor"), false); return page();
      case 1: res.writeHead(503).end("private error body"); return;
      case 2: assert.equal(url.searchParams.get("cursor"), "1-0"); return page([a], "2-0", {has_more: true, lagged: true});
      default: assert.equal(url.searchParams.get("cursor"), "2-0"); return page([a, b], "3-0");
    }
  });
  const result = await f.run(["hello", "--status", "error", "--search", "雪 & timeout", "--version-id", "ver_1"], ({child, stdout}) => {
    if (stdout.includes("exec_2")) child.kill("SIGINT");
  }).finished;
  assert.equal(result.code, 0, result.stderr);
  const events = result.stdout.trim().split("\n").map(JSON.parse);
  assert.deepEqual(events.map(e => e.execution_id), ["exec_1", "exec_2"]);
  assert.deepEqual(events.map(e => e.outcome), ["ok", "error"]);
  assert.match(result.stderr, /Connected to hello/);
  assert.match(result.stderr, /reconnecting/);
  assert.match(result.stderr, /Tail reconnected/);
  assert.match(result.stderr, /events were dropped/);
  assert.match(result.stderr, /Console's execution history/);
  assert.doesNotMatch(result.stderr, /hibana logs/);
  assert.doesNotMatch(result.stdout + result.stderr, /private error body/);
});

test("tail uses the project name and formats empty output, errors, truncation and safe terminal text", async t => {
  let n = 0;
  const f = await fixture(t, () => n++ === 0 ? page() : page([
    event("exec_empty"),
    event("exec_failed", {status: "failed", http_status: null, error: {message: "trap\u001b[2J\u202e"},
      logs: {stdout: "雪\n\u001b]52;c;fixture\u0007\n", stderr: "failure\n", truncated: true}}),
  ], "3-0"));
  // Logging must keep working while local build configuration is being edited.
  await writeFile(join(f.root, "hibana.json"), JSON.stringify({name: "hello", limits: []}));
  const result = await f.run(["--format", "pretty", "--verbose"], ({child, stdout}) => {
    if (stdout.includes("Logs truncated")) child.kill("SIGINT");
  }).finished;
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /HTTP 404 - Ok/);
  assert.match(result.stdout, /23:52:41/);
  assert.match(result.stdout, /JST|GMT\+9/);
  assert.match(result.stdout, /exec_failed.*ver_1/);
  assert.match(result.stdout, /\[stdout\] 雪/);
  assert.match(result.stdout, /\[stderr\] failure/);
  assert.match(result.stdout, /\[exception\] trap/);
  assert.doesNotMatch(result.stdout, /[\u001b\u0007\u202e]/);
  assert.doesNotMatch(result.stdout, /No application output|Logs unavailable|Next page/);
});

test("tail stops promptly on Ctrl+C during an in-flight poll", async t => {
  let n = 0, pending;
  const requested = new Promise(resolve => {pending = resolve;});
  const f = await fixture(t, () => { if (n++ === 0) return page(); pending(); });
  const run = f.run(["hello"]);
  await requested;
  run.child.kill("SIGINT");
  const result = await run.finished;
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.stdout, "");
});

test("tail does not retry revoked credentials and never prints rejection bodies", async t => {
  let n = 0;
  const f = await fixture(t, (_, res) => { if (n++ === 0) return page(); res.writeHead(401).end("private credential details"); });
  const result = await f.run(["hello"]).finished;
  assert.equal(result.code, 1);
  assert.equal(n, 2);
  assert.match(result.stderr, /Authentication failed or expired/);
  assert.doesNotMatch(result.stderr, /private credential details/);
});

test("tail honors stdout backpressure and Ctrl+C when a downstream reader pauses", async t => {
  let polls = 0, paused = false;
  const large = Array.from({length:100}, (_, i) => event(`exec_${i}`, {logs:{stdout:"x".repeat(16384),stderr:"",truncated:false}}));
  const f = await fixture(t, () => ++polls === 1 ? page() : page(large, "2-0", {has_more:true}));
  const run = f.run(["hello"], ({child, stderr}) => {
    if (!paused && stderr.includes("Connected")) { paused = true; child.stdout.pause(); }
  });
  const exited = new Promise(resolve => run.child.once("exit", (code) => {run.child.stdout.resume(); resolve(code);}));
  while (polls < 2) await new Promise(resolve => setTimeout(resolve, 20));
  await new Promise(resolve => setTimeout(resolve, 150));
  assert.equal(polls, 2, "must not fetch another page while stdout is blocked");
  run.child.kill("SIGINT");
  assert.equal(await exited, 0);
  assert.equal((await run.finished).code, 0);
});

test("tail exits cleanly when a downstream pipe closes", async t => {
  let polls = 0;
  const f = await fixture(t, () => ++polls === 1 ? page() : page([event("exec_1")], "2-0"));
  const run = f.run(["hello"], ({child, stderr}) => {
    if (stderr.includes("Connected")) child.stdout.destroy();
  });
  const result = await run.finished;
  assert.equal(result.code, 0, result.stderr);
});

test("tail reports an unsupported API and malformed stream responses", async t => {
  let malformed = false;
  const f = await fixture(t, (_, res) => {
    if (malformed) return {items: []};
    res.writeHead(404).end("private details");
  });
  const unsupported = await f.run(["hello"]).finished;
  assert.equal(unsupported.code, 1);
  assert.match(unsupported.stderr, /Control Plane supports hibana tail/);
  malformed = true;
  const invalid = await f.run(["hello"]).finished;
  assert.equal(invalid.code, 1);
  assert.match(invalid.stderr, /Invalid live tail response/);
});

test("tail retries a response stream interrupted after HTTP headers with the same cursor", async t => {
  let n = 0;
  const f = await fixture(t, (url, res) => {
    if (n++ === 0) return page();
    assert.equal(url.searchParams.get("cursor"), "1-0");
    if (n === 2) {
      res.writeHead(200, {"Content-Length": 10000});
      res.write('{"items":[{"private-body-fragment":');
      setTimeout(() => res.destroy(), 30);
      return;
    }
    return page([event("exec_recovered")], "2-0");
  });
  const result = await f.run(["hello"], ({child, stdout}) => {
    if (stdout.includes("exec_recovered")) child.kill("SIGINT");
  }).finished;
  assert.equal(result.code, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).execution_id, "exec_recovered");
  assert.match(result.stderr, /Tail reconnected/);
  assert.doesNotMatch(result.stderr, /private-body-fragment/);
});
