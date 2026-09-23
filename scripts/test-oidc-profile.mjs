import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";

// Real IdP claims and runtime-role writes in an isolated PostgreSQL database.
export async function testOidcProfile({ api, sql, grant, exchange, idpAdmin, subject, tenant, user, token,
  otherUser, browser, consoleUrl }) {
  const email = async () => (await sql(`SELECT email FROM users WHERE id='${user}'`)).trim();
  const audits = async () => (await sql(`SELECT count(*) FROM audit_logs WHERE target='${user}' AND action='user_profile_updated'`)).trim();
  const original = await idpAdmin(`/users/${subject}`);
  const beforeVersion = (await sql(`SELECT auth_version FROM users WHERE id='${user}'`)).trim();
  const setEmail = value => idpAdmin(`/users/${subject}`, "PUT", { email: value, emailVerified: false });
  const login = async () => {
    const result = await exchange(await grant());
    assert.equal(result.status, 201, JSON.stringify(result.data));
    return result.data.token;
  };
  const scopes = await idpAdmin("/client-scopes");
  const emailScope = scopes.find(scope => scope.name === "email");
  assert.ok(emailScope);
  const mappers = await idpAdmin(`/client-scopes/${emailScope.id}/protocol-mappers/models`);
  const mapper = mappers.find(mapper => mapper.config["claim.name"] === "email");
  assert.ok(mapper);
  const mapperPath = `/client-scopes/${emailScope.id}/protocol-mappers/models/${mapper.id}`;
  try {
    await setEmail("updated@example.invalid");
    const abandoned = await grant();
    assert.ok(abandoned.code);
    assert.equal(await email(), original.email, "provider callback alone must not mutate profiles");
    assert.equal((await exchange(abandoned, randomBytes(32).toString("base64url"))).status, 401);
    assert.equal(await email(), original.email, "bad PKCE must not mutate profiles");
    assert.equal(await audits(), "0");

    // The display update and token issuance must commit or roll back together.
    await sql("ALTER TABLE api_tokens ADD CONSTRAINT fixture_profile_issuance_failure CHECK (name IS DISTINCT FROM 'oidc login') NOT VALID");
    try {
      assert.equal((await exchange(await grant())).status, 500);
      assert.equal(await email(), original.email);
      assert.equal(await audits(), "0");
    } finally {
      await sql("ALTER TABLE api_tokens DROP CONSTRAINT fixture_profile_issuance_failure");
    }

    const oldToken = token;
    token = await login();
    const session = await api("/auth/session", { token });
    assert.equal(session.status, 200);
    assert.equal(session.data.email, "updated@example.invalid", "unverified email is display metadata only");
    assert.equal(session.data.user_id, user);
    assert.equal(session.data.tenant_id, tenant);
    assert.ok(session.data.scopes.includes("admin"));
    assert.equal((await sql(`SELECT auth_version FROM users WHERE id='${user}'`)).trim(), beforeVersion);
    const oldSession = await api("/auth/session", { token: oldToken });
    assert.equal(oldSession.status, 200, "email changes must not revoke existing sessions");
    assert.equal(oldSession.data.email, "updated@example.invalid");
    assert.equal((await api("/users", { token })).data.find(item => item.user_id === user).email, "updated@example.invalid");
    assert.equal(await audits(), "1");
    const detail = JSON.parse((await sql(`SELECT detail FROM audit_logs WHERE target='${user}' AND action='user_profile_updated'`)).trim());
    assert.deepEqual(detail, { via: "oidc", fields: ["email"] });
    token = await login();
    assert.equal(await audits(), "1", "unchanged profiles must not generate audit noise");

    const companion = await api(`/tenants/${tenant}/users`, {
      method: "POST", token,
      body: { email: "updated@example.invalid", role: "member", oidc_subject: "fixture-profile-companion" },
    });
    assert.equal(companion.status, 201, "display emails need not be unique during provisioning");
    await setEmail("shared@example.invalid");
    token = await login();
    await setEmail("updated@example.invalid");
    token = await login();
    assert.equal(await email(), "updated@example.invalid", "sync may share a different member's email");
    assert.equal((await sql(`SELECT role || ':' || oidc_subject FROM users WHERE id='${companion.data.user_id}'`)).trim(), "member:fixture-profile-companion");
    assert.equal((await sql(`SELECT email FROM users WHERE id='${otherUser}'`)).trim(), "other@example.invalid");

    const beforeOptional = await audits();
    await idpAdmin(mapperPath, "PUT", { ...mapper, config: { ...mapper.config, "id.token.claim": "false" } });
    await setEmail("not-in-token@example.invalid");
    token = await login();
    assert.equal(await email(), "updated@example.invalid", "missing claim retains the current label");
    for (const value of ["", " ", "line\nbreak@example.invalid", "a".repeat(321)]) {
      await idpAdmin(mapperPath, "PUT", { ...mapper, protocolMapper: "oidc-hardcoded-claim-mapper",
        config: { "claim.name": "email", "claim.value": value, "jsonType.label": "String", "id.token.claim": "true" } });
      token = await login();
      assert.equal(await email(), "updated@example.invalid", "invalid optional display claim must not break login or replace the label");
    }
    assert.equal(await audits(), beforeOptional);
    await idpAdmin(mapperPath, "PUT", mapper);

    await setEmail("revoked-grant@example.invalid");
    const pending = await grant();
    assert.equal((await api(`/users/${user}/revoke-tokens`, { method: "POST", token })).status, 204);
    assert.equal((await exchange(pending)).status, 401);
    assert.equal(await email(), "updated@example.invalid", "revoked grant must not update the profile");
    assert.equal(await audits(), beforeOptional);

    // The original bug: edit at the IdP, then perform a real console login.
    await setEmail("browser-updated@example.invalid");
    const context = await browser.newContext();
    try {
      const page = await context.newPage();
      await page.goto(consoleUrl);
      await page.getByLabel("テナント", { exact: true }).fill("team");
      await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
      await page.locator("#username").fill("alice");
      await page.locator("#password").fill("fixture-account-password");
      await page.locator("#kc-login").click();
      await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
      assert.equal(await page.locator("summary").innerText(), "browser-updated@example.invalid");
      await page.reload();
      await page.getByRole("button", { name: "ログアウト", exact: true }).waitFor();
      assert.equal(await page.locator("summary").innerText(), "browser-updated@example.invalid");
    } finally {
      await context.close();
    }
    console.log("PASS OIDC profile sync: real email edit and console restore, optional/unverified/duplicate claims, PKCE/revocation/transaction rollback, tenant and identity isolation, audit changes only");
  } finally {
    await idpAdmin(mapperPath, "PUT", mapper);
    await idpAdmin(`/users/${subject}`, "PUT", { email: original.email, emailVerified: original.emailVerified });
  }
  return login();
}
