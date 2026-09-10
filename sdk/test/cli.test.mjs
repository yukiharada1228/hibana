import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, readdir, writeFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";

const cli = process.env.HIBANA_TEST_CLI || fileURLToPath(new URL("../src/cli.mjs", import.meta.url));
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "hibana-cli-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  function invoke(args, { cwd = root, env = {} } = {}) {
    return new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [cli, ...args], {
        cwd, env: { ...process.env, NODE_OPTIONS: "", HIBANA_CONFIG_HOME: join(root, "profiles"),
          HIBANA_RUNTIME_HOME: join(root, "runtimes"), HIBANA_RUNTIME_BIN: "", HIBANA_URL: "", HIBANA_TOKEN: "",
          HIBANA_PROFILE: "", HIBANA_TENANT: "", HIBANA_EMAIL: "", HIBANA_PASSWORD: "", ...env },
        stdio: ["ignore", "pipe", "pipe"],
      });
      let stdout = "", stderr = "";
      const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
      child.once("error", error => { clearTimeout(timer); reject(error); });
      child.stdout.on("data", b => stdout += b);
      child.stderr.on("data", b => stderr += b);
      child.once("close", code => { clearTimeout(timer); resolve({ code, stdout, stderr }); });
    });
  }
  return { root, invoke };
}

test("help is contextual, has examples, and works outside a project without side effects", async t => {
  const f = await fixture(t);
  const rootHelp = await f.invoke([]);
  assert.equal(rootHelp.code, 0, rootHelp.stderr);
  assert.match(rootHelp.stdout, /hibana init my-api/);
  assert.match(rootHelp.stdout, /npm run dev/);
  assert.doesNotMatch(rootHelp.stdout, /--sha256|--kubeconfig/);
  for (const command of ["init", "dev", "build", "deploy", "rollback", "list", "delete", "login", "logout", "runtime", "profile", "secret", "platform"]) {
    const direct = await f.invoke([command, "--help"]);
    const alias = await f.invoke(["help", command]);
    assert.equal(direct.code, 0, direct.stderr);
    assert.equal(alias.code, 0, alias.stderr);
    assert.equal(direct.stdout, alias.stdout);
    assert.equal(direct.stderr, "");
    assert.match(direct.stdout, new RegExp(`Usage: hibana ${command}`));
  }
  const dev = await f.invoke(["dev", "-h"]);
  assert.match(dev.stdout, /--port PORT/);
  assert.doesNotMatch(dev.stdout, /--password-stdin|--kubeconfig|no-runtime-install/);
  for (const args of [["runtime", "install"], ["profile", "use"], ["secret", "put"], ["platform", "install"]]) {
    const direct = await f.invoke([...args, "--help"]);
    assert.equal(direct.code, 0, direct.stderr);
    assert.equal(direct.stdout, (await f.invoke(["help", ...args])).stdout);
  }
  for (const group of ["runtime", "profile", "secret", "platform"]) {
    const result = await f.invoke([group]);
    assert.equal(result.code, 0, result.stderr);
    assert.match(result.stdout, /Commands:/);
  }
  assert.deepEqual(await readdir(f.root), []);
  const version = await f.invoke(["--version"]);
  assert.equal(version.code, 0, version.stderr);
  assert.match(version.stdout, /^hibana \d+\.\d+\.\d+\S*\n$/);
  assert.equal((await f.invoke(["-v"])).stdout, version.stdout);
});

test("invalid commands, flags and arguments fail before any project or network work", async t => {
  const f = await fixture(t), requests = [];
  const server = createServer((req, res) => { requests.push(req.url); res.end("{}"); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const env = { HIBANA_URL: `http://127.0.0.1:${server.address().port}`, HIBANA_TOKEN: "test-token" };
  for (const [args, expected] of [
    [["deply"], /Did you mean 'deploy'/],
    [["dev", "--por", "3000"], /Did you mean '--port'/],
    [["deploy", "--dry-run"], /--dry-run.*not supported.*deploy/],
    [["deploy", "--profile", ""], /--profile.*requires a non-empty NAME/],
    [["dev", "--runtime", ""], /--runtime.*requires a non-empty PATH/],
    [["delete", "hello", "--port", "3000"], /--port.*not supported.*delete/],
    [["dev", "--profile", "production"], /--profile.*not supported.*dev/],
    [["dev", "--no-runtime-install"], /Unknown option/],
    [["runtime", "instal"], /Did you mean 'install'/],
    [["profile", "ues", "prod"], /Unknown profile command/],
    [["dev", "extra"], /Usage: hibana dev/],
    [["deploy", "extra"], /Usage: hibana deploy/],
    [["rollback", "extra"], /Usage: hibana rollback/],
    [["list", "extra"], /Usage: hibana list/],
    [["logout", "extra"], /Usage: hibana logout/],
    [["profile", "remove"], /Usage: hibana profile remove NAME/],
    [["secret", "put"], /Usage: hibana secret put NAME/],
    [["secret", "delete", "API_KEY", "extra"], /Usage: hibana secret delete NAME/],
    [["secret", "list", "API_KEY"], /Usage: hibana secret list/],
    [["secret", "put", "lowercase"], /Secret names must/],
    [["runtime", "install", "extra"], /Usage: hibana runtime install/],
    [["platform", "stop", "extra"], /Usage: hibana platform stop/],
    [["dev", "--port"], /argument missing/],
    [["--url", env.HIBANA_URL], /Choose a command/],
  ]) {
    const result = await f.invoke(args, { env });
    assert.equal(result.code, 1, `${args}: ${result.stderr}`);
    assert.equal(result.stdout, "", args.join(" "));
    assert.match(result.stderr, expected);
    assert.match(result.stderr, /Run hibana.*--help/);
    assert.doesNotMatch(result.stderr, /ENOENT|node:internal|at main|Project configuration/);
  }
  assert.deepEqual(requests, []);
  assert.deepEqual(await readdir(f.root), []);
});

test("missing and malformed project files explain how to recover without exposing configuration values", async t => {
  const f = await fixture(t);
  for (const command of ["dev", "build", "deploy", "delete"]) {
    const result = await f.invoke([command]);
    assert.equal(result.code, 1, result.stderr);
    assert.match(result.stderr, /Project configuration not found/);
    assert.match(result.stderr, /hibana init my-api/);
    assert.match(result.stderr, /--config FILE/);
    assert.doesNotMatch(result.stderr, /ENOENT|node:internal/);
  }
  const path = join(f.root, "broken.json");
  await writeFile(path, '{"vars":{"PASSWORD":"sensitive-test-value"}, bad}');
  const result = await f.invoke(["build", "-c", path]);
  assert.equal(result.code, 1, result.stderr);
  assert.match(result.stderr, /Invalid JSON.*broken.json/);
  assert.doesNotMatch(result.stderr, /sensitive-test-value/);
});

test("init offers executable next steps and recovers from failed dependency installation", async t => {
  const f = await fixture(t);
  const directory = join(f.root, "app with space's");
  const created = await f.invoke(["init", directory, "--no-install"]);
  assert.equal(created.code, 0, created.stderr);
  assert.match(created.stdout, /Next:\n  cd '/);
  assert.ok(created.stdout.includes("space'\\''s'"));
  assert.match(created.stdout, /\n  npm install\n  npm run dev/);
  assert.equal(JSON.parse(await readFile(join(directory, "hibana.json"), "utf8")).main, "src/index.ts");
  const bin = join(f.root, "bin"); await mkdir(bin);
  await writeFile(join(bin, "npm"), `#!${process.execPath}\nprocess.exit(1);\n`, { mode: 0o700 });
  const project = join(f.root, "retry-app");
  const failed = await f.invoke(["init", project], { env: { PATH: bin } });
  assert.equal(failed.code, 1, failed.stderr);
  assert.match(failed.stderr, /Project files are ready, but dependency installation failed/);
  assert.match(failed.stderr, /npm install\n  npm run dev/);
  const config = await readFile(join(project, "hibana.json"), "utf8");
  const repeated = await f.invoke(["init", project, "--no-install"]);
  assert.equal(repeated.code, 1, repeated.stderr);
  assert.match(repeated.stderr, /Directory is not empty/);
  assert.equal(await readFile(join(project, "hibana.json"), "utf8"), config);
  const missing = await f.invoke(["init", join(f.root, "missing-npm")], { env: { PATH: join(f.root, "empty-bin") } });
  assert.equal(missing.code, 1, missing.stderr);
  assert.match(missing.stderr, /Command 'npm' was not found/);
  assert.match(missing.stderr, /npm install\n  npm run dev/);
});

test("platform init creates a portable site with private keys and refuses to overwrite it", async t => {
  const f = await fixture(t);
  const directory = join(f.root, "site");
  const created = await f.invoke(["platform", "init", directory]);
  assert.equal(created.code, 0, created.stderr);
  for (const file of ["kustomization.yaml", "base/kustomization.yaml", "migration/job.yaml", "ingress.yaml", "egress.yaml", "site.yaml", "README.md"]) {
    assert.ok((await readFile(join(directory, file), "utf8")).length, file);
  }
  const credentials = await readFile(join(directory, "control-plane.env"), "utf8");
  const keys = [...credentials.matchAll(/(?:BOOTSTRAP_ADMIN_TOKEN|JOB_SIGNING_KEY|SECRETS_MASTER_KEY)=([a-f0-9]{64})/g)].map(match => match[1]);
  assert.equal(new Set(keys).size, 3);
  for (const key of keys) assert.ok(!created.stdout.includes(key));
  assert.match(await readFile(join(directory, ".gitignore"), "utf8"), /\*\.env/);
  for (const file of ["runtime.env", "control-plane.env", "migration.env"]) assert.equal((await stat(join(directory, file))).mode & 0o777, 0o600);
  const repeated = await f.invoke(["platform", "init", directory]);
  assert.equal(repeated.code, 1, repeated.stderr);
  assert.match(repeated.stderr, /Directory is not empty/);
  assert.equal(await readFile(join(directory, "control-plane.env"), "utf8"), credentials);
});

test("platform dry-run invokes the operator checks and never asks for deletion confirmation", async t => {
  const f = await fixture(t);
  const bin = join(f.root, "bin"); await mkdir(bin);
  await writeFile(join(bin, "python3"), `#!${process.execPath}\nconsole.log('operator-args:' + JSON.stringify(process.argv.slice(2)));\n`, { mode: 0o700 });
  for (const action of ["install", "uninstall", "status"]) {
    const args = ["platform", action, "--kubeconfig", "operator-config", "--context", "onprem", "--dry-run"];
    if (action === "install") args.push("--overlay", "site", "--image", "registry.test/hibana:v1");
    const result = await f.invoke(args, { env: { PATH: bin } });
    assert.equal(result.code, 0, result.stderr);
    assert.match(result.stdout, /Preview: context onprem/);
    const received = JSON.parse(result.stdout.split("operator-args:")[1]);
    assert.ok(received[0].endsWith("platform/remote.py"));
    assert.ok(received.includes("--dry-run"));
    assert.ok(received.includes(action));
    assert.doesNotMatch(result.stderr, /--yes/);
  }
});
