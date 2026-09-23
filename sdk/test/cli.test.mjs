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
  assert.match(rootHelp.stdout, /egress\s+Manage application outbound destinations/);
  assert.doesNotMatch(rootHelp.stdout, /--sha256|--kubeconfig/);
  for (const command of ["init", "dev", "build", "deploy", "rollback", "list", "logs", "delete", "login", "logout", "runtime", "profile", "secret", "platform"]) {
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
    [["delete", "hello", "--force"], /Unknown option/],
    [["delete", "--name", "hello"], /Unknown option/],
    [["platform", "status", "--local"], /Unknown option/],
    [["runtime", "instal"], /Did you mean 'install'/],
    [["profile", "ues", "prod"], /Unknown profile command/],
    [["dev", "extra"], /Usage: hibana dev/],
    [["deploy", "extra"], /Usage: hibana deploy/],
    [["rollback", "extra"], /Usage: hibana rollback/],
    [["list", "extra"], /Usage: hibana list/],
    [["list", "--config", "hibana.json"], /--config.*not supported.*list/],
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

test("NUL vars fail before build, runtime startup or deployment API access", async t => {
  const f = await fixture(t), requests = [];
  const server = createServer((req, res) => { requests.push(req.url); res.end("{}"); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  await writeFile(join(f.root, "hibana.json"), JSON.stringify({
    name: "vars-test", component: "missing.wasm",
    build: { commands: [[process.execPath, "-e", "require('node:fs').writeFileSync('build-started', '')"]] },
    vars: { VALUE: "sensitive-fixture\u0000tail" },
  }));
  const env = { HIBANA_URL: `http://127.0.0.1:${server.address().port}`, HIBANA_TOKEN: "test-token" };
  for (const args of [["build"], ["dev", "--runtime", process.execPath], ["deploy"]]) {
    const result = await f.invoke(args, { env });
    assert.equal(result.code, 1, result.stderr);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /vars\.VALUE.*NUL.*U\+0000/);
    assert.match(result.stderr, /Remove.*hibana\.json/);
    assert.doesNotMatch(result.stderr, /sensitive-fixture|\u0000/);
  }
  assert.deepEqual(requests, []);
  assert.deepEqual(await readdir(f.root), ["hibana.json"]);
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
  for (const file of ["kustomization.yaml", "base/kustomization.yaml", "console/kustomization.yaml", "console/console.yaml", "console/ingress.yaml", "migration/job.yaml", "ingress.yaml", "egress.yaml", "site.yaml", "README.md"]) {
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

test("platform init can provision isolated Keycloak configuration without exposing or replacing credentials", async t => {
  const f = await fixture(t);
  const directory = join(f.root, "site");
  const created = await f.invoke(["platform", "init", directory, "--with-keycloak"]);
  assert.equal(created.code, 0, created.stderr);
  const identity = join(directory, "identity");
  for (const file of ["README.md", "kustomization.yaml", "namespace.yaml", "postgres.yaml", "keycloak.yaml", "config.yaml", "ingress.yaml", "network-policy.yaml", "realm.example.json"]) {
    assert.ok((await readFile(join(identity, file), "utf8")).length, file);
  }
  const credentials = await readFile(join(directory, "control-plane.env"), "utf8");
  const clientSecret = credentials.match(/^OIDC_CLIENT_SECRET=([a-f0-9]{64})$/m)?.[1];
  assert.ok(clientSecret);
  assert.equal(await readFile(join(identity, "client.env"), "utf8"), `HIBANA_OIDC_CLIENT_SECRET=${clientSecret}\n`);
  const secrets = [clientSecret];
  for (const file of ["database.env", "bootstrap-admin.env"]) {
    const contents = await readFile(join(identity, file), "utf8");
    secrets.push(contents.match(/PASSWORD=([a-f0-9]{64})/)?.[1]);
  }
  assert.ok(secrets.every(Boolean));
  assert.equal(new Set(secrets).size, 3);
  for (const secret of secrets) assert.ok(!(created.stdout + created.stderr).includes(secret));
  for (const file of ["database.env", "bootstrap-admin.env", "client.env", "imports/hibana-realm.json"]) {
    assert.equal((await stat(join(identity, file))).mode & 0o777, 0o600);
  }
  assert.equal((await stat(join(identity, "imports"))).mode & 0o777, 0o700);
  const ignored = await readFile(join(identity, ".gitignore"), "utf8");
  assert.match(ignored, /\*\.env/);
  assert.match(ignored, /\/imports\//);
  // Identity is independently deployed, not part of Hibana's managed inventory.
  assert.doesNotMatch(await readFile(join(directory, "kustomization.yaml"), "utf8"), /identity/);
  const realm = JSON.parse(await readFile(join(identity, "imports/hibana-realm.json"), "utf8"));
  assert.equal(realm.users, undefined);
  assert.equal(realm.clients[0].secret, "${HIBANA_OIDC_CLIENT_SECRET}");
  const repeated = await f.invoke(["platform", "init", directory, "--with-keycloak"]);
  assert.equal(repeated.code, 1);
  assert.match(repeated.stderr, /Directory is not empty/);
  assert.equal(await readFile(join(directory, "control-plane.env"), "utf8"), credentials);
  const another = join(f.root, "another");
  assert.equal((await f.invoke(["platform", "init", another, "--with-keycloak"])).code, 0);
  assert.notEqual(await readFile(join(another, "identity/client.env"), "utf8"), `HIBANA_OIDC_CLIENT_SECRET=${clientSecret}\n`);
  const wrongAction = await f.invoke(["platform", "install", "--with-keycloak"]);
  assert.equal(wrongAction.code, 1);
  assert.match(wrongAction.stderr, /--with-keycloak.*not supported/);
});

test("application logs support named apps, execution IDs, cursors and safe terminal output", async t => {
  const f = await fixture(t), calls = [];
  const cursor = "2026-09-23T01:02:03+00:00,exec_1";
  const item = { execution_id: "exec_1", component_id: "cmp_1", version_id: "ver_1", status: "failed", created_at: "2026-09-23T01:02:03Z",
    logs: { stdout: "雪\n\u001b[2J\u001b]52;c;fixture\u0007", stderr: "failed\n", truncated: true } };
  let denied = false;
  const server = createServer((req, res) => {
    calls.push(req.url);
    assert.equal(req.headers.authorization, "Bearer fixture-read");
    if (denied) { res.writeHead(403).end("private rejection details"); return; }
    const url = new URL(req.url, "http://fixture.invalid");
    res.setHeader("content-type", "application/json");
    if (url.pathname === "/components") return res.end(JSON.stringify([{name:"logs-app",component_id:"cmp_1"}]));
    if (url.pathname === "/executions/exec_1") return res.end(JSON.stringify(item));
    assert.equal(url.pathname, "/components/cmp_1/logs");
    if (url.searchParams.has("before")) {
      assert.equal(url.searchParams.get("before"), cursor);
      assert.equal(url.searchParams.get("errors_only"), "true");
      res.end(JSON.stringify({items:[],next_cursor:null}));
    } else res.end(JSON.stringify({items:[item],next_cursor:cursor}));
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const env = {HIBANA_URL:`http://127.0.0.1:${server.address().port}`,HIBANA_TOKEN:"fixture-read"};
  const human = await f.invoke(["logs", "logs-app"], {env});
  assert.equal(human.code, 0, human.stderr);
  assert.match(human.stdout, /exec_1.*ver_1.*failed/);
  assert.match(human.stdout, /雪/);
  assert.match(human.stdout, /truncated at 16 KiB/);
  assert.match(human.stdout, /Next page/);
  assert.doesNotMatch(human.stdout, /[\u001b\u0007]/);
  const detail = await f.invoke(["logs", "--execution", "exec_1", "--json"], {env});
  assert.equal(detail.code, 0, detail.stderr);
  assert.deepEqual(JSON.parse(detail.stdout), {items:[item],next_cursor:null});
  const next = await f.invoke(["logs", "logs-app", "--before", cursor, "--errors-only"], {env});
  assert.equal(next.code, 0, next.stderr);
  assert.match(next.stdout, /No executions/);
  await writeFile(join(f.root, "hibana.json"), JSON.stringify({name:"logs-app",component:"unused.wasm"}));
  assert.equal((await f.invoke(["logs"], {env})).code, 0);
  const count = calls.length;
  assert.equal((await f.invoke(["logs", "--execution", "exec_1", "--before", cursor], {env})).code, 1);
  assert.equal(calls.length, count);
  denied = true;
  const rejected = await f.invoke(["logs", "--execution", "exec_1"], {env});
  assert.equal(rejected.code, 1);
  assert.match(rejected.stderr, /Permission denied/);
  assert.doesNotMatch(rejected.stderr, /private rejection/);
});
