import assert from "node:assert/strict";

// Real Keycloak, PostgreSQL, and browser cookies, through the console proxy.
export async function testConsoleSession({ page, context, consoleUrl, secondary, grant, sql }) {
  const origin = new URL(consoleUrl).origin;
  const session = await page.evaluate(async () => (await fetch("/api/auth/session", {
    headers: { "x-hibana-console": "1" },
  })).json());
  const secure = new URL(consoleUrl).protocol === "https:";
  const cookie = (await context.cookies()).find(cookie => /^(?:__Host-)?hibana_session_/.test(cookie.name));
  assert.ok(session.expires_in_ms > 0);
  assert.ok(cookie?.httpOnly);
  assert.equal(cookie.sameSite, "Strict");
  assert.equal(cookie.secure, secure);
  assert.equal(cookie.name.startsWith("__Host-"), secure);
  assert.equal(cookie.domain, "127.0.0.1");
  assert.equal(cookie.path, "/");
  assert.equal(await page.evaluate(() => document.cookie), "");
  assert.deepEqual(await page.evaluate(() => [localStorage.length, sessionStorage.length]), [0, 0]);
  await page.getByRole("link", { name: "利用状況", exact: true }).click();
  const route = page.url();
  await page.clock.install({ time: new Date(Date.now() + 86_400_000) });
  await page.reload();
  await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
  assert.equal(page.url(), route);
  assert.equal((await context.cookies()).find(value => value.name === cookie.name).expires, cookie.expires);

  const request = (path, { method = "GET", body, headers = {} } = {}) => fetch(secondary + path, {
    method,
    headers: { cookie: `${cookie.name}=${cookie.value}`, origin, "x-hibana-console": "1", "x-hibana-session": session.token_id,
      "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  assert.equal((await request("/components")).status, 200, "cookie works across replicas");
  for (const headers of [
    { origin: "https://evil.invalid" }, { origin: "null" }, { origin: "http://127.0.0.1:1" },
    { "x-hibana-console": "" }, { "sec-fetch-site": "same-site" },
  ]) {
    assert.equal((await request("/components", { headers })).status, 403);
    assert.equal((await request("/auth/logout", { method: "POST", headers })).status, 403);
  }
  for (const headers of [{ "x-hibana-session": "" }, { "x-hibana-session": "a-different-login" }, { authorization: "Bearer invalid" }]) {
    assert.equal((await request("/auth/logout", { method: "POST", headers })).status, 401);
  }
  const restored = await request("/auth/session");
  assert.equal(restored.status, 200, "denied requests never revoke the session");
  assert.equal(restored.headers.get("cache-control"), "no-store");
  assert.equal((await restored.json()).expires_at, session.expires_at);

  const tab = await context.newPage();
  assert.match(session.tenant_id, /^ten_[a-f0-9]+$/);
  await sql(`UPDATE tenants SET status='suspended' WHERE id='${session.tenant_id}'`);
  assert.equal((await request("/components")).status, 403, "suspension still denies tenant resources");
  assert.equal((await request("/auth/session")).status, 200, "a suspended tenant can inspect its own session to log out");
  await tab.goto(consoleUrl);
  await tab.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
  await tab.getByRole("button", { name: "ログアウト", exact: true }).click();
  await tab.getByRole("heading", { name: "コンソールにログイン" }).waitFor();
  assert.ok(!(await context.cookies()).some(value => value.name === cookie.name));
  await page.reload();
  await page.getByRole("heading", { name: "コンソールにログイン" }).waitFor();
  assert.equal((await request("/auth/session")).status, 401, "logout revokes server-side credentials too");
  await tab.close();
  await sql(`UPDATE tenants SET status='active' WHERE id='${session.tenant_id}'`);

  const pending = await grant();
  const exchange = (headers) => fetch(secondary + "/auth/oidc/browser/exchange", {
    method: "POST", headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify({ code: pending.code, code_verifier: pending.verifier }),
  });
  assert.equal((await exchange({ origin: "https://evil.invalid", "x-hibana-console": "1" })).status, 403);
  assert.equal((await exchange({ "x-hibana-console": "1" })).status, 403);
  const issued = await exchange({ origin, "x-hibana-console": "1" });
  assert.equal(issued.status, 201, "CSRF rejection does not consume a valid grant");
  const metadata = await issued.json();
  assert.equal(metadata.token, undefined, "browser exchange never exposes a bearer credential in JSON");
  const setCookie = issued.headers.get("set-cookie");
  assert.match(setCookie, /HttpOnly/);
  assert.match(setCookie, /SameSite=Strict/);
  const pair = setCookie.split(";", 1)[0];
  const [name, value] = pair.split("=");
  await context.addCookies([{ name, value, url: consoleUrl, httpOnly: true, secure, sameSite: "Strict" }]);
  await page.reload();
  await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
  assert.match(metadata.token_id, /^tok_[a-f0-9]+$/);
  await sql(`UPDATE api_tokens SET expires_at=now()-interval '1 second' WHERE id='${metadata.token_id}'`);
  await page.reload();
  await page.getByRole("heading", { name: "コンソールにログイン" }).waitFor();
  assert.equal((await request("/auth/session", { headers: { cookie: pair, "x-hibana-session": metadata.token_id } })).status, 401);
  console.log("PASS console session: reload, route/new-tab restore, clock skew, HttpOnly isolation, fixed expiry, cross-replica lookup, CSRF rejection, stale-tab binding, server-side logout and expiry");
}
