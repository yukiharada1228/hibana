import assert from "node:assert/strict";
import { randomBytes, createHash } from "node:crypto";
import { join } from "node:path";
import { browserLogin } from "../sdk/src/oidc.mjs";
import { createSamlBrokerFixture } from "./saml-broker-fixture.mjs";

export async function testSamlBroker({ idp, api, base, secondary, consoleUrl, callbackUrl, cliBase, browser, folder, clearRate, bootstrapToken }) {
  for (const endpoint of [base, secondary]) {
    const config = await api("/auth/config", { endpoint });
    assert.equal(config.data.console_url, consoleUrl);
  }
  const fixture = await createSamlBrokerFixture(idp);
  const employee = fixture.people[0];
  const created = await api("/admin/tenants", { method: "POST", token: bootstrapToken, body: {
    slug: "saml-team", name: "SAML team", admin_email: "corporate-shared@example.invalid",
    admin_oidc_subject: employee.subject,
  } });
  assert.equal(created.status, 201);
  const { tenant_id: tenant, admin_user_id: user } = created.data;

  async function withPage(run) {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.setDefaultTimeout(15_000);
    page.setDefaultNavigationTimeout(15_000);
    // Decode only synthetic fixture messages in memory, never persist SAML
    // assertions, cookies, authorization codes or access tokens in artifacts.
    const saml = { requests: [], responses: [] };
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const form = new URLSearchParams(request.postData() || "");
      if (request.url() === `${fixture.upstream}/protocol/saml` && form.has("SAMLRequest"))
        saml.requests.push(Buffer.from(form.get("SAMLRequest"), "base64").toString());
      if (request.url() === fixture.endpoint && form.has("SAMLResponse"))
        saml.responses.push(Buffer.from(form.get("SAMLResponse"), "base64").toString());
    });
    try { return await run(page, saml); }
    finally { await context.close(); }
  }
  async function authenticate(page, username = "employee") {
    await page.locator(`#social-${fixture.alias}`).click();
    await page.waitForURL((url) => url.href.startsWith(`${fixture.upstream}/`));
    await page.locator("#username").fill(username);
    await page.locator("#password").fill(fixture.password);
    await page.locator("#kc-login").click();
  }
  function assertSaml(saml, username = "employee") {
    assert.equal(saml.requests.length, 1, "browser sends a real SAML AuthnRequest to the corporate IdP");
    assert.equal(saml.responses.length, 1, "corporate IdP returns a real SAML response to the broker");
    // Boolean assertions also keep the XML out of failure diagnostics.
    assert.ok(/<\w*:AuthnRequest\b/.test(saml.requests[0]));
    assert.ok(/<\w*:SignatureValue>/.test(saml.requests[0]));
    assert.ok(/<\w*:Response\b/.test(saml.responses[0]));
    assert.ok(/<\w*:Assertion\b/.test(saml.responses[0]));
    assert.equal((saml.responses[0].match(/<\w*:SignatureValue>/g) || []).length, 2,
      "both the response and assertion are signed");
    assert.ok(saml.responses[0].includes(fixture.upstream));
    assert.ok(saml.responses[0].includes(fixture.entityId));
    assert.ok(saml.responses[0].includes(fixture.people.find((person) => person.name === username).source));
  }
  async function start(tenantSlug = "saml-team") {
    await clearRate();
    const verifier = randomBytes(32).toString("base64url");
    const state = randomBytes(32).toString("base64url");
    const response = await api("/auth/oidc/start", { method: "POST", endpoint: secondary, body: {
      tenant_slug: tenantSlug, redirect_uri: consoleUrl, state,
      code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    } });
    assert.equal(response.status, 200);
    return { url: response.data.authorization_url, verifier, state };
  }
  async function grant(username = "employee", tenantSlug = "saml-team") {
    const flow = await start(tenantSlug);
    return withPage(async (page, saml) => {
      await page.goto(flow.url);
      await authenticate(page, username);
      await page.waitForURL((url) => url.origin === new URL(consoleUrl).origin);
      assertSaml(saml, username);
      const returned = await page.evaluate(() => window.__oidcReturn || location.href);
      const fields = new URLSearchParams(new URL(returned).hash.slice(1));
      assert.equal(fields.get("oidc_state"), flow.state);
      return { code: fields.get("oidc_code"), error: fields.get("oidc_error"), verifier: flow.verifier };
    });
  }
  const exchange = (grant) => api("/auth/oidc/exchange", { method: "POST", endpoint: secondary,
    body: { code: grant.code, code_verifier: grant.verifier } });
  const first = await grant();
  assert.ok(first.code);
  const login = await exchange(first);
  assert.equal(login.status, 201);
  let token = login.data.token;
  const session = await api("/auth/session", { token });
  assert.equal(session.status, 200);
  assert.equal(session.data.tenant_id, tenant);
  assert.equal(session.data.user_id, user);
  assert.equal((await exchange(first)).status, 401);
  console.log("PASS SAML: signed AuthnRequest/Response/Assertion through a real corporate IdP; broker OIDC subject maps to Hibana; cross-replica one-use exchange");

  await clearRate();
  await withPage(async (page, saml) => {
    await page.goto(consoleUrl);
    await page.getByLabel("テナント", { exact: true }).fill("saml-team");
    await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
    await authenticate(page);
    await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
    assertSaml(saml);
    assert.equal(await page.evaluate(() => sessionStorage.getItem("hibana.oidc.pending")), null);
    assert.ok(!page.url().includes("oidc_code"));
    await page.reload();
    await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
    await page.screenshot({ path: join(folder, "saml-console.png"), fullPage: true });
    await page.getByRole("button", { name: "ログアウト", exact: true }).click();
    await page.getByRole("heading", { name: "コンソールにログイン" }).waitFor();
  });
  await clearRate();
  await withPage(async (page, saml) => {
    const cli = await browserLogin({ tenant: "saml-team", request: async (path, options) => {
      const result = await api(path, { ...options, endpoint: cliBase });
      assert.ok(result.status < 300, `CLI ${path}: ${result.status}`);
      return result.data;
    } }, { log: () => {}, timeoutMs: 30_000, open: async (url) => {
      await page.goto(url);
      await authenticate(page);
    } });
    assertSaml(saml);
    assert.equal((await api("/auth/session", { token: cli.token })).data.user_id, user);
    assert.equal((await api("/auth/logout", { method: "POST", token: cli.token })).status, 204);
    assert.equal((await api("/auth/session", { token: cli.token })).status, 401);
  });
  console.log("PASS SAML: real console login/logout and CLI loopback login with OIDC-only Control Planes");

  const outsider = await grant("outsider");
  assert.equal(outsider.error, "login_failed", "same email cannot grant another subject membership");
  assert.equal(outsider.code, null);
  assert.equal((await grant("employee", "team")).error, "login_failed");

  // Change the identity attribute without re-signing the response. A successful
  // baseline alone would not prove that signature verification is enabled.
  const tampered = await start();
  await withPage(async (page) => {
    let changed = false;
    let hibanaCallback = false;
    page.on("request", (request) => {
      if (request.url().startsWith(`${callbackUrl}?`)) hibanaCallback = true;
    });
    await page.route(fixture.endpoint, async (route) => {
      const form = new URLSearchParams(route.request().postData());
      const xml = Buffer.from(form.get("SAMLResponse"), "base64").toString();
      assert.ok(xml.includes(employee.source));
      form.set("SAMLResponse", Buffer.from(xml.replaceAll(employee.source, fixture.people[1].source)).toString("base64"));
      changed = true;
      await route.continue({ postData: form.toString() });
    });
    await page.goto(tampered.url);
    const rejected = page.waitForResponse((response) => response.url() === fixture.endpoint);
    await authenticate(page);
    assert.equal((await rejected).status(), 400, "broker must reject a tampered SAML signature");
    await page.getByText("Invalid signature in response from identity provider.", { exact: true }).waitFor();
    assert.ok(changed);
    assert.equal(hibanaCallback, false);
    await page.screenshot({ path: join(folder, "saml-signature-rejected.png"), fullPage: true });
  });
  console.log("PASS SAML: tampered signed assertion rejected by broker; same-email outsider and wrong Hibana tenant rejected");

  const pending = await grant();
  assert.equal((await api("/auth/logout-all", { method: "POST", token })).status, 204);
  assert.equal((await api("/auth/session", { token })).status, 401);
  assert.equal((await exchange(pending)).status, 401);
  token = (await exchange(await grant())).data.token;
  assert.ok(token);
  assert.equal((await api(`/users/${user}`, { method: "DELETE", token })).status, 204);
  assert.equal((await api("/auth/session", { token })).status, 401);
  assert.equal((await grant()).error, "login_failed");
  console.log("PASS SAML: Hibana logout-all revokes sessions/pending grants; disabled membership cannot log in even after successful corporate SAML authentication");
}
