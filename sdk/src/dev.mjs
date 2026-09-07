import { spawn } from "node:child_process";
import { access, readFile, writeFile, chmod, mkdir } from "node:fs/promises";
import { constants, watch } from "node:fs";
import { parseEnv } from "node:util";
import { dirname, resolve, relative, join, delimiter } from "node:path";
import { fileURLToPath } from "node:url";
import { loadConfig } from "./config.mjs";
import { run } from "./process.mjs";

export function shouldRebuild(config, file) {
  const path = resolve(config.root, file);
  if (path === config.path || path === join(config.root, ".dev.vars")) return true;
  if (file.split(/[\\/]/).some(part => ["node_modules", ".hibana", ".git", "target", "vendor"].includes(part))) return false;
  if (config.component && path === resolve(config.root, config.component)) return !config.build;
  if (!config.build?.watch) return true;
  return config.build.watch.some(input => {
    const within = relative(resolve(config.root, input), path);
    return within === "" || (within !== ".." && !within.startsWith("../") && !within.startsWith("..\\"));
  });
}

export async function dev(config, options, build) {
  const port = Number(options.port || 8787);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("port must be 1..65535");
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
  let runtime = options.runtime || process.env.HIBANA_RUNTIME_BIN;
  if (!runtime) {
    const candidates = [join(repo, "target/release/faas-worker"), ...(process.env.PATH || "").split(delimiter).map(dir => join(dir, "faas-worker"))];
    for (const candidate of candidates) { try { await access(candidate, constants.X_OK); runtime = candidate; break; } catch {} }
    if (!runtime) {
      try { await access(join(repo, "crates/worker/Cargo.toml")); }
      catch { throw new Error("Install faas-worker or set HIBANA_RUNTIME_BIN to its path"); }
      console.log("Building the Wasmtime development runtime…");
      await run("cargo", ["build", "--release", "-p", "faas-worker"], { cwd: repo });
      runtime = join(repo, "target/release/faas-worker");
    }
  }
  const settings = join(config.root, ".hibana/dev-settings.json");
  let child, watcher, timer, stopping = false, rebuilding = false, dirty = false;
  async function stopChild() {
    const current = child; child = undefined;
    if (current && current.exitCode === null && current.signalCode === null) {
      await new Promise(done => {
        const killTimer = setTimeout(() => current.kill("SIGKILL"), 65000);
        current.once("exit", () => { clearTimeout(killTimer); done(); });
        current.kill("SIGTERM");
      });
    }
  }
  async function start() {
    const artifact = await build(config);
    let local = {};
    try { local = parseEnv(await readFile(join(config.root, ".dev.vars"), "utf8")); }
    catch (error) { if (error.code !== "ENOENT") throw error; }
    if (stopping) return;
    await stopChild();
    await mkdir(dirname(settings), { recursive: true });
    await writeFile(settings, JSON.stringify({ vars: { ...config.vars, ...local }, resources: config.resources }), { mode: 0o600 });
    await chmod(settings, 0o600);
    child = spawn(runtime, ["--dev-component", artifact, "--dev-settings", settings, "--bind", `127.0.0.1:${port}`], { stdio: "inherit" });
    child.once("error", error => { console.error(`Runtime: ${error.message}`); process.exitCode = 1; stop(); });
    child.once("exit", (code, signal) => {
      if (!stopping && code !== 0 && signal !== "SIGTERM") { console.error(`Runtime exited (${signal || code})`); process.exitCode = 1; stop(); }
    });
  }
  async function rebuild() {
    if (stopping) return;
    if (rebuilding) { dirty = true; return; }
    rebuilding = true;
    try { config = await loadConfig(config.path); await start(); }
    catch (error) { console.error(`Build failed: ${error.message}`); }
    finally { rebuilding = false; if (dirty) { dirty = false; await rebuild(); } }
  }
  let finished;
  const completion = new Promise(done => { finished = done; });
  async function stop() {
    if (stopping) return;
    stopping = true; clearTimeout(timer); watcher?.close();
    await stopChild(); finished();
  }
  process.once("SIGINT", stop); process.once("SIGTERM", stop);
  try {
    await start();
    if (!options["no-watch"]) {
      watcher = watch(config.root, { recursive: true }, (_, file) => {
        if (!file || !shouldRebuild(config, file)) return;
        clearTimeout(timer); timer = setTimeout(rebuild, 200);
      });
      console.log("Watching project files; rebuilds restart the Wasmtime server.");
    }
    await completion;
  } finally { await stop(); process.removeListener("SIGINT", stop); process.removeListener("SIGTERM", stop); }
}
