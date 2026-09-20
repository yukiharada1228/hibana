import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { randomBytes, createHash, X509Certificate } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, readFile, open } from "node:fs/promises";
import { resolve, join, extname } from "node:path";
import { createRequire } from "node:module";
import { setTimeout as sleep } from "node:timers/promises";
import { runCommand } from "./bounded-process.mjs";
import { browserLogin } from "../sdk/src/oidc.mjs";
import { testTokenIssuance } from "./test-token-issuance.mjs";
import { testIdentityChanges } from "./test-identity-changes.mjs";
import { testIdentityRevocation } from "./test-identity-revocation.mjs";
import { issueFixtureToken } from "./test-api-credentials.mjs";
import { testOidcRateLimit } from "./test-oidc-rate-limit.mjs";
import { testConsoleSession } from "./test-console-session.mjs";
const { chromium } = createRequire(
  new URL("../console/package.json", import.meta.url),
)("@playwright/test");

const pg = process.env.OIDC_TEST_PG,
  redis = process.env.OIDC_TEST_REDIS,
  keycloak = process.env.OIDC_TEST_KEYCLOAK;
for (const [value, kind] of [
  [pg, "pg"],
  [redis, "redis"],
  [keycloak, "keycloak"],
])
  assert.match(value || "", new RegExp(`^hibana-oidc-${kind}-[0-9]+$`));
async function port(container, internal) {
  return (await runCommand("docker", ["port", container, `${internal}/tcp`]))
    .trim()
    .split(":")
    .at(-1);
}
const postgres = `postgres://postgres@127.0.0.1:${await port(pg, 5432)}/hibana_oidc`;
const runtimeDb = postgres.replace("postgres@", "faas_app:faas_app@");
const redisUrl = `redis://127.0.0.1:${await port(redis, 6379)}`;
const https = process.env.HIBANA_TEST_HTTPS === "1";
const scheme = https ? "https" : "http";
const idp = `${scheme}://127.0.0.1:${await port(keycloak, https ? 8443 : 8080)}`;
const tlsOptions = https ? {
  cert: await readFile(process.env.OIDC_TEST_TLS_CERT),
  key: await readFile(process.env.OIDC_TEST_TLS_KEY),
} : undefined;
const listen = async (server) => {
  await new Promise((ok, fail) => {
    server.once("error", fail);
    server.listen(0, "127.0.0.1", ok);
  });
  return server.address().port;
};
async function vacantPort() {
  const s = createServer();
  const p = await listen(s);
  await new Promise((ok) => s.close(ok));
  return p;
}
const cpPort = await vacantPort(),
  secondPort = await vacantPort();
const base = `http://127.0.0.1:${cpPort}`,
  secondary = `http://127.0.0.1:${secondPort}`;
const consoleRoot = resolve("console/dist");
const serve = async (req, res) => {
  try {
    if (req.url.startsWith("/api/")) {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const upstream = await fetch(base + req.url.slice(4), {
        method: req.method,
        headers: {
          "content-type": "application/json",
          ...Object.fromEntries(["cookie", "origin", "sec-fetch-site", "x-hibana-console", "x-hibana-session"]
            .filter(key => req.headers[key]).map(key => [key, req.headers[key]])),
          ...(req.headers.authorization
            ? { authorization: req.headers.authorization }
            : {}),
        },
        body: ["GET", "HEAD"].includes(req.method)
          ? undefined
          : Buffer.concat(chunks),
        redirect: "manual",
      });
      res
        .writeHead(upstream.status, Object.fromEntries(upstream.headers))
        .end(Buffer.from(await upstream.arrayBuffer()));
      return;
    }
    const path = new URL(req.url, "http://fixture.invalid").pathname;
    const file = resolve(consoleRoot, path === "/" ? "index.html" : `.${path}`);
    assert.ok(file.startsWith(consoleRoot + "/"));
    res.setHeader(
      "Content-Type",
      {
        ".html": "text/html",
        ".js": "application/javascript",
        ".css": "text/css",
      }[extname(file)] || "text/plain",
    );
    res.end(await readFile(file));
  } catch {
    res.writeHead(404).end();
  }
};
const site = https ? createHttpsServer(tlsOptions, serve) : createServer(serve);
const consoleUrl = `${scheme}://127.0.0.1:${await listen(site)}/`;
const callbackUrl = https ? `${consoleUrl}api/auth/oidc/callback` : `${base}/auth/oidc/callback`;
const cliBase = https ? `${consoleUrl}api` : base;
const folder = resolve(".local/verification/oidc");
await mkdir(folder, { recursive: true });
const log = await open(join(folder, "control-plane.log"), "w");
const binary = resolve(
  process.env.CARGO_TARGET_DIR || "target",
  "debug/hibana-control-plane",
);
const env = {
  ...process.env,
  OIDC_ISSUER_URL: `${idp}/realms/hibana`,
  OIDC_CLIENT_ID: "hibana",
  OIDC_CLIENT_SECRET: "fixture-client-secret",
  OIDC_CALLBACK_URL: callbackUrl,
  OIDC_CONSOLE_URL: consoleUrl,
  OIDC_ALLOW_INSECURE_HTTP: https ? "false" : "true",
  ...(https ? { OIDC_CA_CERT_FILE: process.env.OIDC_TEST_CA } : {}),
  OIDC_SESSION_TTL_SECS: "900",
  // The rate-limit fixture forwards distinct client IPs through loopback only.
  TRUSTED_PROXY_CIDRS: "127.0.0.1/32",
  DATABASE_URL: runtimeDb,
  MIGRATION_DATABASE_URL: postgres,
  RUN_MIGRATIONS: "false",
  REDIS_URL: redisUrl,
  BOOTSTRAP_ADMIN_TOKEN: "fixture-bootstrap",
  S3_ENDPOINT: "http://127.0.0.1:1",
  S3_ACCESS_KEY: "fixture",
  S3_SECRET_KEY: "fixture",
  WORKER_HTTP_URL: "http://127.0.0.1:1",
  JOB_SIGNING_KEY: "01".repeat(32),
  JOB_SIGNING_KID: "fixture",
  SECRETS_MASTER_KEY: "02".repeat(32),
  SECRETS_MASTER_KID: "fixture",
  RUST_LOG: "info",
};
delete env.AUTH_MODE;
delete env.APP_BIND_ADDR;
const processes = [];
let browser;
async function eventually(check, message) {
  for (let i = 0; i < 120; i++) {
    try {
      if (await check()) return;
    } catch {}
    await sleep(500);
  }
  throw new Error(message);
}
async function launch(port, overrides = {}) {
  const cp = spawn(binary, [], {
    env: {
      ...env,
      ...overrides,
      BIND_ADDR: `127.0.0.1:${port}`,
      INTERNAL_BIND_ADDR: `127.0.0.1:${await vacantPort()}`,
    },
    stdio: ["ignore", log.fd, log.fd],
  });
  processes.push(cp);
  await eventually(
    async () =>
      cp.exitCode === null &&
      (await fetch(`http://127.0.0.1:${port}/healthz`)).ok,
    "Control Plane did not start",
  );
  return cp;
}
const sql = (query) =>
  runCommand("docker", [
    "exec",
    pg,
    "psql",
    "-XqAt",
    "-U",
    "postgres",
    "-d",
    "hibana_oidc",
    "-v",
    "ON_ERROR_STOP=1",
    "-c",
    query,
  ]);
async function clearRate() {
  await runCommand("docker", [
    "exec",
    redis,
    "redis-cli",
    "DEL",
    "rl:oidc:global",
    "rl:oidc:ip:127.0.0.1",
  ]);
}
async function api(
  path,
  { method = "GET", token, body, endpoint = base } = {},
) {
  const response = await fetch(endpoint + path, {
    method,
    redirect: "manual",
    headers: {
      "content-type": "application/json",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let data;
  try {
    data = await response.json();
  } catch {}
  return { status: response.status, data, headers: response.headers };
}
async function grant(tenant = "team", username = "alice", mutations = {}) {
  await clearRate();
  const verifier = randomBytes(32).toString("base64url"),
    state = randomBytes(32).toString("base64url");
  const request = {
    tenant_slug: tenant,
    state,
    code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    redirect_uri: consoleUrl,
    ...mutations,
  };
  const start = await api("/auth/oidc/start", {
    method: "POST",
    body: request,
    endpoint: secondary,
  });
  assert.equal(start.status, 200, JSON.stringify(start.data));
  const ctx = await browser.newContext(),
    page = await ctx.newPage();
  let callback;
  page.on("request", (req) => {
    if (req.url().startsWith(`${callbackUrl}?`))
      callback = req.url();
  });
  await page.goto(start.data.authorization_url);
  await page.locator("#username").fill(username);
  await page.locator("#password").fill("fixture-account-password");
  await page.locator("#kc-login").click();
  await page.waitForURL((url) => url.origin === new URL(consoleUrl).origin, {
    timeout: 15_000,
  });
  // The app will remove the fragment; capture it before any app JS runs.
  const result = await page.evaluate(
    () => window.__oidcReturn || location.href,
  );
  await ctx.close();
  const params = new URLSearchParams(new URL(result).hash.slice(1));
  assert.equal(params.get("oidc_state"), state);
  return {
    code: params.get("oidc_code"),
    error: params.get("oidc_error"),
    verifier,
    callback,
  };
}
async function exchange(g, verifier = g.verifier) {
  return api("/auth/oidc/exchange", {
    method: "POST",
    body: { code: g.code, code_verifier: verifier },
    endpoint: secondary,
  });
}

try {
  await eventually(
    async () =>
      (await fetch(`${idp}/realms/master/.well-known/openid-configuration`)).ok,
    "Keycloak did not start",
  );
  await runCommand(binary, ["--migrate-only"], { env });
  // Provisioning must write a complete identity in the initial INSERT, without
  // relying on a later linking UPDATE. This constraint is test-only.
  await sql(`ALTER TABLE users ADD CONSTRAINT fixture_oidc_creation_required
    CHECK (oidc_issuer IS NOT NULL AND oidc_subject IS NOT NULL)`);
  const credentials = new URLSearchParams({
    client_id: "admin-cli",
    grant_type: "password",
    username: "fixture-admin",
    password: "fixture-admin-password",
  });
  const admin = await (
    await fetch(`${idp}/realms/master/protocol/openid-connect/token`, {
      method: "POST",
      body: credentials,
    })
  ).json();
  assert.ok(admin.access_token);
  const alice = "00000000-0000-4000-8000-000000000001",
    bob = "00000000-0000-4000-8000-000000000002";
  const realm = await fetch(`${idp}/admin/realms`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${admin.access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      realm: "hibana",
      enabled: true,
      sslRequired: "none",
      duplicateEmailsAllowed: true,
      clients: [
        {
          clientId: "hibana",
          secret: env.OIDC_CLIENT_SECRET,
          enabled: true,
          protocol: "openid-connect",
          publicClient: false,
          standardFlowEnabled: true,
          directAccessGrantsEnabled: false,
          redirectUris: [env.OIDC_CALLBACK_URL],
          attributes: { "pkce.code.challenge.method": "S256" },
        },
      ],
      users: [
        [alice, "alice"],
        [bob, "bob"],
      ].map(([id, username]) => ({
        id,
        username,
        email: "shared@example.invalid",
        firstName: username,
        lastName: "Fixture",
        emailVerified: true,
        enabled: true,
        credentials: [
          {
            type: "password",
            value: "fixture-account-password",
            temporary: false,
          },
        ],
      })),
    }),
  });
  assert.equal(realm.status, 201, await realm.text());
  await launch(cpPort);
  await launch(secondPort);
  // Trust only this fixture's browser certificate key, without changing the
  // workstation trust store. CP and CLI verify the chain using the private CA.
  const spki = https ? createHash("sha256").update(new X509Certificate(tlsOptions.cert)
    .publicKey.export({ type: "spki", format: "der" })).digest("base64") : undefined;
  browser = await chromium.launch({ headless: true,
    args: spki ? [`--ignore-certificate-errors-spki-list=${spki}`] : [],
  });
  // Capture one-time handoffs before the real console strips them.
  const newContext = browser.newContext.bind(browser);
  browser.newContext = async (...args) => {
    const ctx = await newContext(...args);
    await ctx.addInitScript(() => {
      window.__oidcReturn = location.href;
    });
    return ctx;
  };
  const created = await api("/admin/tenants", {
    method: "POST",
    token: env.BOOTSTRAP_ADMIN_TOKEN,
    body: {
      slug: "team",
      name: "OIDC team",
      admin_email: "shared@example.invalid",
      admin_oidc_subject: alice,
    },
  });
  assert.equal(created.status, 201, JSON.stringify(created.data));
  const { tenant_id: tenant, admin_user_id: user } = created.data;
  if (https) {
    const untrustedPort = await vacantPort();
    const untrusted = await launch(untrustedPort, { OIDC_CA_CERT_FILE: "" });
    const rejected = await api("/auth/oidc/start", {
      endpoint: `http://127.0.0.1:${untrustedPort}`, method: "POST", body: {
        tenant_slug: "team", redirect_uri: consoleUrl,
        state: randomBytes(32).toString("base64url"),
        code_challenge: randomBytes(32).toString("base64url"),
      },
    });
    assert.equal(rejected.status, 503, "an untrusted IdP certificate must fail closed");
    const exited = once(untrusted, "exit");
    untrusted.kill("SIGTERM");
    await exited;
    console.log("PASS HTTPS IdP trust: configured private CA accepted; untrusted CA rejected");
  }
  assert.equal((await api("/admin/tenants", {
    method: "POST", token: env.BOOTSTRAP_ADMIN_TOKEN,
    body: { slug: "team", name: "Replacement", admin_email: "replacement@example.invalid", admin_oidc_subject: bob },
  })).status, 409, "repeated bootstrap must report an existing tenant without rebinding its admin");
  assert.equal((await sql(`SELECT oidc_subject FROM users WHERE id='${user}'`)).trim(), alice);
  assert.equal((await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action='tenant_created'`)).trim(), "1");
  const otherTenant = await api("/admin/tenants", {
    method: "POST",
    token: env.BOOTSTRAP_ADMIN_TOKEN,
    body: {
      slug: "other-team",
      name: "Other Team",
      admin_email: "other@example.invalid",
      admin_oidc_subject: "fixture-other-subject",
    },
  });
  assert.equal(otherTenant.status, 201);
  assert.equal(
    (
      await api("/admin/tenants", {
        method: "POST",
        token: env.BOOTSTRAP_ADMIN_TOKEN,
        body: {
          slug: "weak",
          name: "Weak",
          admin_email: "weak@example.invalid",
          admin_password: "a",
        },
      })
    ).status,
    400,
  );
  await testOidcRateLimit({ base, secondary, consoleUrl, redis });
  assert.equal((await api("/auth/login", { method: "POST", body: {
    tenant_slug: "team", email: "shared@example.invalid", password: "removed",
  } })).status, 404, "the password login route is removed");
  assert.equal((await api("/admin/tenants", { method: "POST", token: env.BOOTSTRAP_ADMIN_TOKEN,
    body: { slug: "password-rejected", name: "Fixture", admin_email: "fixture@example.invalid",
      admin_oidc_subject: alice, admin_password: "removed-password" },
  })).status, 400, "password fields cannot be silently accepted");
  const otherAdmin = { data: await (await issueFixtureToken(sql, {
    tenant_slug: "other-team", email: "other@example.invalid",
  })).json() };
  console.log("PASS password login and password provisioning removed; OIDC is mandatory");
  const g = await grant();
  assert.ok(g.code);
  const issued = await exchange(g);
  assert.equal(issued.status, 201, JSON.stringify(issued.data));
  const usersBeforeConflict = (await sql(`SELECT count(*) FROM users WHERE tenant_id='${tenant}'`)).trim();
  const auditsBeforeConflict = (await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action='user_created'`)).trim();
  for (const body of [
    { email: "duplicate-identity@example.invalid", role: "member", oidc_subject: alice },
    { email: "shared@example.invalid", role: "member", oidc_subject: "fixture-unused-subject" },
  ]) {
    assert.equal((await api(`/tenants/${tenant}/users`, {
      method: "POST", token: issued.data.token, body,
    })).status, 409, "duplicate email/identity must not create a partial user");
  }
  assert.equal((await sql(`SELECT count(*) FROM users WHERE tenant_id='${tenant}'`)).trim(), usersBeforeConflict);
  assert.equal((await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action='user_created'`)).trim(), auditsBeforeConflict);
  const linked = await api(`/tenants/${tenant}/users`, {
    method: "POST", token: issued.data.token,
    body: { email: "link-conflict@example.invalid", role: "member", oidc_subject: "fixture-link-conflict" },
  });
  assert.equal(linked.status, 201);
  assert.equal((await api(`/users/${linked.data.user_id}/oidc`, {
    method: "PUT", token: issued.data.token, body: { oidc_subject: alice },
  })).status, 409, "linking an existing identity must fail without changing the target");
  assert.equal((await sql(`SELECT oidc_subject FROM users WHERE id='${linked.data.user_id}'`)).trim(), "fixture-link-conflict");
  assert.equal((await sql(`SELECT count(*) FROM audit_logs WHERE target='${linked.data.user_id}' AND action='user_oidc_linked'`)).trim(), "0");
  console.log("PASS provisioning inserts complete OIDC identities and rolls back duplicate email/subject conflicts");
  assert.equal(
    (await exchange(g)).status,
    401,
    "handoff is one-use across replicas",
  );
  assert.equal(
    (await fetch(g.callback, { redirect: "manual" })).status,
    401,
    "provider callback is one-use",
  );
  let token = issued.data.token;
  assert.equal(
    (await api("/auth/session", { token })).data.email,
    "shared@example.invalid",
  );
  assert.equal((await api("/auth/session", { token })).data.tenant_id, tenant);
  console.log(
    "PASS real Keycloak code flow, provider PKCE, issuer/subject binding and cross-replica one-use exchange",
  );

  const bad = await grant();
  assert.equal(
    (await exchange(bad, randomBytes(32).toString("base64url"))).status,
    401,
  );
  assert.equal((await exchange(bad)).status, 401);
  assert.equal(
    (await grant("team", "bob")).error,
    "login_failed",
    "same email must not link another subject",
  );
  assert.equal(
    (await grant("other-team")).error,
    "login_failed",
    "membership is tenant-specific",
  );
  const pending = await grant();
  assert.equal(
    (await api(`/users/${user}/revoke-tokens`, { method: "POST", token }))
      .status,
    204,
  );
  assert.equal((await api("/auth/session", { token })).status, 401);
  assert.equal(
    (await exchange(pending)).status,
    401,
    "revocation invalidates in-progress login grants",
  );
  token = (await exchange(await grant())).data.token;
  console.log(
    "PASS PKCE mismatch, same-email takeover, tenant boundary, user-wide revocation and pending-grant revocation",
  );

  const ctx = await browser.newContext(),
    page = await ctx.newPage();
  await page.goto(consoleUrl);
  await page.getByLabel("テナント", { exact: true }).fill("team");
  await page
    .getByRole("button", { name: "組織のアカウントでログイン", exact: true })
    .click();
  await page.locator("#username").fill("alice");
  await page.locator("#password").fill("fixture-account-password");
  await page.locator("#kc-login").click();
  await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
  assert.equal(
    await page.evaluate(() => sessionStorage.getItem("hibana.oidc.pending")),
    null,
  );
  assert.ok(!page.url().includes("oidc_code"));
  await page.screenshot({ path: join(folder, "console.png"), fullPage: true });
  await testConsoleSession({ page, context: ctx, consoleUrl, secondary, grant, sql });
  await ctx.close();
  const cliContext = await browser.newContext(),
    cliPage = await cliContext.newPage();
  const cliToken = await browserLogin(
    {
      tenant: "team",
      request: async (path, options) => {
        const result = await api(path, { ...options, endpoint: cliBase });
        assert.ok(result.status < 300, JSON.stringify(result.data));
        return result.data;
      },
    },
    {
      log: () => {},
      open: async (url) => {
        await cliPage.goto(url);
        await cliPage.locator("#username").fill("alice");
        await cliPage.locator("#password").fill("fixture-account-password");
        await cliPage.locator("#kc-login").click();
      },
    },
  );
  assert.equal(
    (await api("/auth/session", { token: cliToken.token })).status,
    200,
  );
  await cliContext.close();
  console.log(
    "PASS real console redirect/exchange/logout and CLI browser login",
  );

  for (const body of [
    { email: "removed@example.invalid", role: "member", password: "removed-password" },
    { email: "removed@example.invalid", role: "member", oidc_subject: "fixture-new", password: "removed-password" },
    { email: "removed@example.invalid", role: "member", oidc_subject: "" },
  ]) assert.equal((await api(`/tenants/${tenant}/users`, { method: "POST", token, body })).status, 400);
  token = await testTokenIssuance({
    api, sql, url: base, tenant, user, subject: alice, token,
    login: async () => (await exchange(await grant())).data.token,
  });
  token = await testIdentityChanges({
    api, sql, url: base, tenant, user, subject: alice, unboundSubject: bob, token,
    login: async () => (await exchange(await grant())).data.token,
  });
  token = await testIdentityRevocation({
    api, sql, pg, tenant, user, token,
    login: async () => (await exchange(await grant())).data.token,
  });
  assert.equal((await grant("team", "bob")).error, "login_failed", "rejected identity writes cannot grant the unbound account access");
  const scoped = await api("/tokens", {
    method: "POST",
    token,
    body: { user_id: user, scopes: ["read"], name: "login", ttl_secs: 60 },
  });
  assert.equal(scoped.status, 201);
  assert.equal(
    (
      await api("/auth/session", {
        token: scoped.data.token,
        endpoint: secondary,
      })
    ).status,
    200,
  );
  assert.equal((await sql(`SELECT auth_method FROM api_tokens WHERE id='${scoped.data.token_id}'`)).trim(), "api");
  console.log("PASS scoped API credentials work across replicas");

  const otherUser = otherTenant.data.admin_user_id;
  assert.equal((await api("/users", { token: scoped.data.token })).status, 403);
  assert.equal(
    (await api(`/users/${otherUser}/oidc`, {
      method: "PUT", token, body: { oidc_subject: bob },
    })).status,
    404,
    "an administrator cannot link a user in another tenant",
  );
  assert.equal(
    (await api("/users", { token: otherAdmin.data.token })).data[0].user_id,
    otherUser,
  );
  assert.equal(
    (await api(`/users/${otherUser}/oidc`, {
      method: "PUT", token: otherAdmin.data.token, body: { oidc_subject: bob },
    })).status,
    204,
  );
  assert.equal((await api("/auth/session", { token: otherAdmin.data.token })).status, 401);
  let relinked = await exchange(await grant("other-team", "bob"));
  assert.equal(relinked.status, 201);
  const beforeLogout = await grant("other-team", "bob");
  await sql("UPDATE tenants SET status='suspended' WHERE slug='other-team'");
  assert.equal((await api("/components", { token: relinked.data.token })).status, 403);
  assert.equal(
    (await api("/auth/logout-all", { method: "POST", token: relinked.data.token })).status,
    204,
  );
  assert.equal((await api("/auth/session", { token: relinked.data.token })).status, 401);
  await sql("UPDATE tenants SET status='active' WHERE slug='other-team'");
  assert.equal((await exchange(beforeLogout)).status, 401);
  relinked = await exchange(await grant("other-team", "bob"));
  assert.equal(relinked.status, 201);
  assert.equal(
    (await api(`/users/${otherUser}`, { method: "DELETE", token: relinked.data.token })).status,
    204,
  );
  assert.equal((await api("/auth/session", { token: relinked.data.token })).status, 401);
  assert.equal((await grant("other-team", "bob")).error, "login_failed");
  assert.equal((await api("/auth/session", { token })).status, 200);
  console.log("PASS explicit identity linking, admin tenant boundaries, logout-all and account disabling");

  await sql(`UPDATE users SET role='member' WHERE id='${user}'`);
  assert.ok(
    !(await api("/auth/session", { token })).data.scopes.includes("admin"),
  );
  assert.equal((await api("/users", { token })).status, 403);
  await sql(`UPDATE users SET deleted_at=now() WHERE id='${user}'`);
  assert.equal((await api("/auth/session", { token })).status, 401);
  assert.equal(
    (await api("/auth/session", { token: scoped.data.token })).status,
    401,
  );
  console.log(
    "PASS OIDC and scoped API credentials enforce role demotion and soft deletion",
  );
  if (process.env.HIBANA_TEST_SAML === "1") {
    const { testSamlBroker } = await import("./test-saml-broker.mjs");
    await testSamlBroker({ idp, api, base, secondary, consoleUrl, callbackUrl, cliBase, browser, folder, clearRate,
      bootstrapToken: env.BOOTSTRAP_ADMIN_TOKEN });
  }
} finally {
  await browser?.close();
  for (const child of processes) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGTERM");
      await once(child, "exit");
    }
  }
  site.closeAllConnections();
  await new Promise((ok) => site.close(ok));
  await log.close();
}
