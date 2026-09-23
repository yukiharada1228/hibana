import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { run } from "../src/process.mjs";

const module = new URL("../src/process.mjs", import.meta.url).href;
const cli = fileURLToPath(new URL("../src/cli.mjs", import.meta.url));

async function workspace(t) {
  const cwd = await mkdtemp(join(tmpdir(), "hibana-process-"));
  t.after(() => rm(cwd, { recursive: true, force: true }));
  return cwd;
}

function launch(t, args, cwd) {
  const child = spawn(process.execPath, args, { cwd, detached: true, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  child.stdout.on("data", bytes => { output += bytes; });
  child.stderr.on("data", bytes => { output += bytes; });
  const finished = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  t.after(async () => {
    // run() owns a separate group; fixture PIDs make failed-test cleanup explicit.
    for (const pid of [child.pid, ...[...output.matchAll(/PID:(\d+)/g)].map(m => Number(m[1]))]) {
      try { process.kill(-pid, "SIGKILL"); } catch {}
      try { process.kill(pid, "SIGKILL"); } catch {}
    }
    await finished;
  });
  async function waitFor(text) {
    const deadline = Date.now() + 8000;
    while (!output.includes(text)) {
      assert.ok(Date.now() < deadline && child.exitCode === null && child.signalCode === null, output);
      await delay(20);
    }
  }
  async function exit() {
    let timer;
    try {
      return await Promise.race([finished, new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`Process did not stop: ${output}`)), 8000);
      })]);
    } finally { clearTimeout(timer); }
  }
  return { child, waitFor, exit, output: () => output };
}

async function assertGone(pid) {
  const deadline = Date.now() + 3000;
  for (;;) {
    try { process.kill(pid, 0); }
    catch (error) { assert.equal(error.code, "ESRCH"); return; }
    assert.ok(Date.now() < deadline, `process ${pid} survived cancellation`);
    await delay(20);
  }
}

for (const signal of ["SIGINT", "SIGTERM"]) {
  test(`${signal} reaches command descendants and waits for cleanup`, async t => {
    const cwd = await workspace(t);
    await writeFile(join(cwd, "grandchild.mjs"), `process.on('SIGTERM',()=>{});process.on('SIGINT',()=>{});console.log('PID:'+process.pid);console.log('GRANDCHILD_READY');setInterval(()=>{},1000);`);
    await writeFile(join(cwd, "command.mjs"), `import{spawn}from'node:child_process';import{writeFileSync}from'node:fs';console.log('PID:'+process.pid);spawn(process.execPath,['grandchild.mjs'],{stdio:'inherit'});for(const signal of ['SIGINT','SIGTERM'])process.on(signal,()=>setTimeout(()=>{writeFileSync('cleaned',signal);process.exit(0)},100));setInterval(()=>{},1000);`);
    const code = `import{run}from${JSON.stringify(module)};try{await run(process.execPath,['command.mjs'],{stopTimeoutMs:1000})}catch(e){console.log(e.name);process.exitCode=1}`;
    const app = launch(t, ["--input-type=module", "-e", code], cwd);
    await app.waitFor("GRANDCHILD_READY");
    app.child.kill(signal);
    assert.deepEqual(await app.exit(), { code: 1, signal: null }, app.output());
    assert.equal(await readFile(join(cwd, "cleaned"), "utf8"), signal);
    assert.match(app.output(), /AbortError/);
    for (const match of app.output().matchAll(/PID:(\d+)/g)) await assertGone(Number(match[1]));
  });
}

test("uncooperative commands are killed after the grace period", async t => {
  const cwd = await workspace(t);
  const code = `import{run}from${JSON.stringify(module)};try{await run(process.execPath,['-e',"process.on('SIGTERM',()=>{});console.log('PID:'+process.pid);console.log('READY');setInterval(()=>{},1000)"],{stopTimeoutMs:100})}catch(e){process.exitCode=1}`;
  const app = launch(t, ["--input-type=module", "-e", code], cwd);
  await app.waitFor("READY");
  app.child.kill("SIGTERM");
  assert.deepEqual(await app.exit(), { code: 1, signal: null }, app.output());
  await assertGone(Number(app.output().match(/PID:(\d+)/)[1]));
});

test("run preserves literal arguments, bounds captured output and removes listeners", async () => {
  const before = [process.listenerCount("SIGINT"), process.listenerCount("SIGTERM")];
  const literal = "$(must-not-run); `nor-this`";
  assert.deepEqual(await run(process.execPath, ["-e", "console.log(process.argv[1]);console.error('diagnostic')", literal], { capture: true }), { stdout: literal + "\n", stderr: "diagnostic\n" });
  await assert.rejects(run("hibana-nonexistent-test-command", [], { capture: true }), /was not found/);
  await assert.rejects(run(process.execPath, ["-e", "process.stdout.write('x'.repeat(2048));setInterval(()=>{},1000)"], { capture: true, maxBuffer: 1024 }), /exceeds 1024 bytes/);
  await assert.rejects(run(process.execPath, ["-e", "setInterval(()=>{},1000)"], { timeout: 100 }), /timed out/);
  const controller = new AbortController();
  const running = run(process.execPath, ["-e", "setInterval(()=>{},1000)"], { signal: controller.signal });
  controller.abort();
  await assert.rejects(running, { name: "AbortError" });
  await assert.rejects(run(process.execPath, [], { signal: controller.signal }), { name: "AbortError" });
  assert.deepEqual([process.listenerCount("SIGINT"), process.listenerCount("SIGTERM")], before);
});

test("platform SIGTERM waits for the Python operator's cleanup", async t => {
  const cwd = await workspace(t);
  await mkdir(join(cwd, "sdk/platform"), { recursive: true });
  await writeFile(join(cwd, "sdk/platform/kubernetes.py"), `import os, pathlib, signal, time\ndef stop(signum, frame):\n    raise KeyboardInterrupt\nsignal.signal(signal.SIGTERM, stop)\nprint('PID:' + str(os.getpid()), flush=True)\ntry:\n    print('OPERATOR_READY', flush=True)\n    while True: time.sleep(1)\nexcept KeyboardInterrupt:\n    time.sleep(0.1)\n    pathlib.Path('released').write_text('yes')\n`);
  const app = launch(t, [cli, "platform", "status", "--source", cwd], cwd);
  await app.waitFor("OPERATOR_READY");
  app.child.kill("SIGTERM");
  assert.deepEqual(await app.exit(), { code: 1, signal: null }, app.output());
  assert.equal(await readFile(join(cwd, "released"), "utf8"), "yes");
  await assertGone(Number(app.output().match(/PID:(\d+)/)[1]));
});

for (const phase of ["initial build", "rebuild"]) {
  test(`dev cancels its real ${phase} and leaves no runtime or next build`, async t => {
    const cwd = await workspace(t);
    const rebuild = phase === "rebuild";
    const runtime = join(cwd, "runtime.mjs");
    await writeFile(runtime, `#!${process.execPath}\nconsole.log('PID:'+process.pid);console.log('RUNTIME_READY');process.on('SIGTERM',()=>process.exit(0));setInterval(()=>{},1000);`, { mode: 0o700 });
    await writeFile(join(cwd, "build.mjs"), `import{existsSync,writeFileSync}from'node:fs';if(${rebuild}&&!existsSync('app.wasm')){writeFileSync('app.wasm',Buffer.from([0,97,115,109,13,0,1,0]));}else{process.on('SIGTERM',()=>{writeFileSync('build-stopped','yes');process.exit(0)});console.log('PID:'+process.pid);console.log('BUILD_READY');setInterval(()=>{},1000);}`);
    await writeFile(join(cwd, "source.txt"), "initial");
    await writeFile(join(cwd, "hibana.json"), JSON.stringify({ name: "cancel-test", component: "app.wasm", build: { commands: [[process.execPath, "build.mjs"], [process.execPath, "-e", "console.log('NEXT_COMMAND')"]], watch: ["source.txt"] } }));
    const app = launch(t, [cli, "dev", "--runtime", runtime], cwd);
    if (rebuild) {
      await app.waitFor("RUNTIME_READY");
      await app.waitFor("Watching");
      await writeFile(join(cwd, "source.txt"), "changed");
    }
    await app.waitFor("BUILD_READY");
    app.child.kill("SIGTERM");
    assert.deepEqual(await app.exit(), { code: 0, signal: null }, app.output());
    assert.equal(await readFile(join(cwd, "build-stopped"), "utf8"), "yes");
    assert.equal(app.output().match(/RUNTIME_READY/g)?.length || 0, rebuild ? 1 : 0);
    assert.equal(app.output().match(/NEXT_COMMAND/g)?.length || 0, rebuild ? 1 : 0);
    for (const match of app.output().matchAll(/PID:(\d+)/g)) await assertGone(Number(match[1]));
  });
}
