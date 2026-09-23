// Runs against the disposable real Control Plane, Worker, Redis and Wasm fixture.
import assert from "node:assert/strict";
import {spawn} from "node:child_process";
import {resolve} from "node:path";
import {setTimeout as sleep} from "node:timers/promises";

export async function testLiveTail({url, token, invoke}) {
  const child = spawn(process.execPath, [resolve("sdk/src/cli.mjs"), "tail", "runtime-boundaries", "--format", "json"], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {...process.env, HIBANA_URL: url, HIBANA_TOKEN: token, HIBANA_PROFILE: ""},
  });
  let stdout = "", stderr = "";
  child.stdout.on("data", data => {stdout += data;});
  child.stderr.on("data", data => {stderr += data;});
  const done = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({code, signal}));
  });
  async function until(check) {
    for (let i = 0; i < 250; i++) {
      if (check()) return;
      assert.equal(child.exitCode, null, stderr);
      await sleep(20);
    }
    assert.fail(`live tail deadline: ${stderr}`);
  }
  try {
    await until(() => stderr.includes("Connected to runtime-boundaries"));
    assert.equal(stdout, "", "tail must not replay stored executions");
    assert.equal((await invoke("GET", "/logs")).status, 200);
    assert.equal((await invoke("GET", "/trap")).status, 502);
    await until(() => stdout.trim().split("\n").filter(Boolean).length >= 2);
    child.kill("SIGINT");
    assert.equal((await done).code, 0, stderr);
    const events = stdout.trim().split("\n").map(JSON.parse);
    assert.equal(events.length, 2);
    assert.deepEqual(events.map(e => e.outcome), ["ok", "error"]);
    assert.equal(events[0].logs.stdout, "hello 雪\n");
    assert.match(events[1].logs.stderr, /runtime regression fixture/);
    assert.equal(new Set(events.map(e => e.execution_id)).size, 2);
    assert.doesNotMatch(stderr, /events were dropped/);
    console.log("PASS real CLI live tail: starts now, follows committed Wasm output/errors, JSON lines, Ctrl+C");
  } finally {
    if (child.exitCode === null) child.kill("SIGKILL");
    await done;
  }
}
