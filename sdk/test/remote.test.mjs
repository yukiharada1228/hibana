import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile, stat, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const cli = process.env.HIBANA_TEST_CLI || fileURLToPath(new URL("../src/cli.mjs", import.meta.url));
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "hibana-remote-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "settings");
  const project = join(root, "project");
  await mkdir(project);
  function invoke(args, { cwd = project, input = "", env = {} } = {}) {
    return new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [cli, ...args], { cwd, env: {
        ...process.env, HIBANA_CONFIG_HOME: home, HIBANA_URL: "", HIBANA_PROFILE: "", HIBANA_TOKEN: "",
        HIBANA_TENANT: "", HIBANA_EMAIL: "", HIBANA_PASSWORD: "", HIBANA_INGRESS_DOMAIN: "", ...env,
      }, stdio: ["pipe", "pipe", "pipe"] });
      let output = "";
      child.stdout.on("data", b => output += b);
      child.stderr.on("data", b => output += b);
      child.once("error", reject);
      child.once("exit", code => resolve({ code, output }));
      child.stdin.end(input);
    });
  }
  const calls = [];
  async function server(token, { tls, redirect } = {}) {
    let components = [];
    const handler = async (req, res) => {
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      const body = Buffer.concat(chunks).toString();
      calls.push({ url: req.url, method: req.method, auth: req.headers.authorization, body, token });
      res.setHeader("Content-Type", "application/json");
      if (req.url === "/api/auth/login") {
        if (JSON.parse(body).password === "wrong") { res.writeHead(401); res.end("{}"); return; }
        res.end(JSON.stringify({ token })); return;
      }
      if (req.headers.authorization !== `Bearer ${token}`) { res.writeHead(401); res.end("{}"); return; }
      if (redirect) { res.writeHead(302, { location: redirect }); res.end(); return; }
      if (req.url === "/api/components") {
        if (req.method === "POST") { components = [{ name: JSON.parse(body).name, component_id: "cmp" }]; res.end(JSON.stringify(components[0])); }
        else res.end(JSON.stringify(components));
      } else if (req.url.endsWith("/rollback")) res.end('{"active_version_id":"old-version"}');
      else { if (req.method === "DELETE" && req.url === "/api/components/cmp") components = []; res.end("{}"); }
    };
    const instance = tls ? createHttpsServer(tls, handler) : createServer(handler);
    await new Promise(resolve => instance.listen(0, "127.0.0.1", resolve));
    t.after(() => new Promise(resolve => instance.close(resolve)));
    return `${tls ? "https" : "http"}://127.0.0.1:${instance.address().port}/api`;
  }
  return { root, home, project, invoke, calls, server };
}

test("remote login, deploy, rollback, secrets and deletion work across directories with only API credentials", async t => {
  const f = await fixture(t);
  const url = await f.server("tenant-token");
  const login = await f.invoke(["login", "--profile", "onprem", "--url", url, "--tenant", "team", "--email", "dev@example.com", "--ingress-domain", "apps.example.com", "--password-stdin"], { input: "test-password\n" });
  assert.equal(login.code, 0, login.output);
  assert.ok(!login.output.includes("tenant-token") && !login.output.includes("test-password"));
  assert.deepEqual(JSON.parse(f.calls[0].body), { tenant_slug: "team", email: "dev@example.com", password: "test-password" });
  assert.equal(f.calls[0].auth, undefined);
  assert.equal((await stat(join(f.home, "profiles.json"))).mode & 0o777, 0o600);
  await assert.rejects(stat(join(f.project, ".hibana/auth.json")), { code: "ENOENT" });
  await writeFile(join(f.project, "app.wasm"), Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]));
  await writeFile(join(f.project, "hibana.json"), JSON.stringify({ name: "hello", component: "app.wasm", vars: { VALUE: "versioned" } }));
  const deployed = await f.invoke(["deploy", "--profile", "onprem", "--version", "1.0.0"]);
  assert.equal(deployed.code, 0, deployed.output);
  assert.match(deployed.output, /hello.team.apps.example.com/);
  assert.match(f.calls.at(-1).body, /application\/wasm/);
  assert.match(f.calls.at(-1).body, /versioned/);
  for (const args of [["rollback", "--version", "0.9.0"], ["secret", "put", "API_KEY"], ["secret", "allow-deploy", "API_KEY"], ["secret", "delete", "API_KEY"]]) {
    const r = await f.invoke(args, { input: "secret-value\n" }); assert.equal(r.code, 0, r.output);
  }
  const listed = await f.invoke(["list"], { cwd: f.root });
  assert.equal(listed.code, 0, listed.output); assert.match(listed.output, /hello/);
  const deleted = await f.invoke(["delete", "hello", "--profile", "onprem", "--yes"], { cwd: f.root });
  assert.equal(deleted.code, 0, deleted.output);
  assert.match((await f.invoke(["list"], { cwd: f.root })).output, /\[\]/);
  assert.ok(f.calls.slice(1).every(c => c.auth === "Bearer tenant-token"));
});

test("profiles isolate servers, support selection/logout and never leak saved tokens to overrides", async t => {
  const f = await fixture(t);
  const first = await f.server("first-token"), second = await f.server("second-token");
  for (const [name, url] of [["prod", first], ["staging", second]]) {
    const r = await f.invoke(["login", "--profile", name, "--url", url, "--tenant", "team", "--email", "dev@example.com"], { env: { HIBANA_PASSWORD: "password" } });
    assert.equal(r.code, 0, r.output);
  }
  const list = await f.invoke(["profile", "list"]);
  assert.match(list.output, /\* staging/); assert.ok(!list.output.includes("token"));
  assert.equal((await f.invoke(["profile", "use", "prod"])).code, 0);
  assert.equal((await f.invoke(["list"])).code, 0); assert.equal(f.calls.at(-1).token, "first-token");
  assert.equal((await f.invoke(["list"], { env: { HIBANA_PROFILE: "staging" } })).code, 0); assert.equal(f.calls.at(-1).token, "second-token");
  assert.equal((await f.invoke(["list", "--profile", "prod"], { env: { HIBANA_URL: second } })).code, 0); assert.equal(f.calls.at(-1).token, "first-token");
  const before = f.calls.length;
  for (const args of [["list", "--profile", "unknown"], ["list", "--profile", "prod", "--url", second]]) assert.notEqual((await f.invoke(args)).code, 0);
  assert.equal(f.calls.length, before);
  const saved = await readFile(join(f.home, "profiles.json"), "utf8");
  assert.notEqual((await f.invoke(["login", "--profile", "prod"], { env: { HIBANA_PASSWORD: "wrong" } })).code, 0);
  assert.equal(await readFile(join(f.home, "profiles.json"), "utf8"), saved);
  const logout = await f.invoke(["logout", "--profile", "prod"]); assert.equal(logout.code, 0, logout.output);
  assert.notEqual((await f.invoke(["list"])).code, 0);
  assert.equal((await f.invoke(["list", "--profile", "staging"])).code, 0);
  assert.equal((await f.invoke(["profile", "remove", "prod"])).code, 0);
  const state = JSON.parse(await readFile(join(f.home, "profiles.json"), "utf8"));
  assert.equal(state.current, undefined); assert.equal(state.profiles.staging.token, "second-token");
});

test("management API redirects are rejected before contacting the next server", async t => {
  const f = await fixture(t);
  const destination = await f.server("other-token");
  const url = await f.server("redirect-token", { redirect: destination + "/components" });
  const result = await f.invoke(["list"], { env: { HIBANA_URL: url, HIBANA_TOKEN: "redirect-token" } });
  assert.notEqual(result.code, 0);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].token, "redirect-token");
});

test("HTTPS verifies certificates and supports an explicitly trusted on-prem CA", async t => {
  const f = await fixture(t);
  const config = join(f.root, "ca.conf"), key = join(f.root, "key.pem"), cert = join(f.root, "ca.pem");
  await writeFile(config, "[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n[dn]\nCN=localhost\n[v3]\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,digitalSignature,keyEncipherment,keyCertSign\n");
  execFileSync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-config", config, "-keyout", key, "-out", cert], { stdio: "ignore" });
  const url = await f.server("tls-token", { tls: { key: await readFile(key), cert: await readFile(cert) } });
  const env = { HIBANA_URL: url, HIBANA_TOKEN: "tls-token", NODE_TLS_REJECT_UNAUTHORIZED: "1", NODE_EXTRA_CA_CERTS: "" };
  assert.notEqual((await f.invoke(["list"], { env })).code, 0);
  assert.equal(f.calls.length, 0);
  const trusted = await f.invoke(["list"], { env: { ...env, NODE_EXTRA_CA_CERTS: cert } });
  assert.equal(trusted.code, 0, trusted.output);
  assert.equal(f.calls[0].auth, "Bearer tls-token");
});

test("no implicit local target, unsafe URLs rejected, explicit CI token and legacy credentials supported", async t => {
  const f = await fixture(t);
  assert.match((await f.invoke(["list"])).output, /No Hibana server selected/);
  for (const url of ["http://remote.example.com", "https://user:password@example.com", "https://example.com?q=secret", "https://example.com#fragment"])
    assert.notEqual((await f.invoke(["list", "--url", url], { env: { HIBANA_TOKEN: "never-send" } })).code, 0);
  const url = await f.server("ci-token");
  assert.equal((await f.invoke(["list"], { env: { HIBANA_URL: url, HIBANA_TOKEN: "ci-token" } })).code, 0);
  await mkdir(join(f.project, ".hibana"));
  await writeFile(join(f.project, ".hibana/auth.json"), JSON.stringify({ url, token: "ci-token" }));
  assert.equal((await f.invoke(["list"])).code, 0);
  assert.notEqual((await f.invoke(["list", "--profile", "missing"])).code, 0);
});
