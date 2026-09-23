import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { requestAfterChange } from "./test-token-issuance.mjs";

export async function testIdentityChanges({ api, sql, url, tenant, user, subject, unboundSubject, token, login }) {
  const created = await api(`/tenants/${tenant}/users`, {
    method: "POST", token,
    body: { email: "identity-target@example.invalid", role: "admin", oidc_subject: "fixture-identity-target" },
  });
  assert.equal(created.status, 201);
  const target = created.data.user_id;
  const mutate = async (path, method = "POST", body) => {
    assert.equal((await api(path, { method, token, body })).status, 204);
  };
  const hash = () => createHash("sha256").update(token).digest("hex");
  const cases = [
    { name: "logout-all", change: () => mutate("/auth/logout-all"), status: 401 },
    { name: "single-token logout", change: () => mutate("/auth/logout"), status: 401 },
    { name: "caller disabled", change: () => mutate(`/users/${user}`, "DELETE"),
      restore: () => sql(`UPDATE users SET deleted_at=NULL WHERE id='${user}'`), status: 401 },
    { name: "identity changed", change: () => mutate(`/users/${user}/oidc`, "PUT", { oidc_subject: subject }), status: 401 },
    { name: "caller expired", change: () => sql(`UPDATE api_tokens SET expires_at=now()-interval '1 second' WHERE token_hash='${hash()}'`), status: 401 },
    { name: "role downgraded", change: () => sql(`UPDATE users SET role='member' WHERE id='${user}'`),
      restore: () => sql(`UPDATE users SET role='admin' WHERE id='${user}'`), status: 403 },
    { name: "Admin scope removed", change: () => sql(`UPDATE api_tokens SET scopes=ARRAY['read'] WHERE token_hash='${hash()}'`), status: 403 },
  ];
  for (const operation of [
    { path: `/tenants/${tenant}/users`, method: "POST", body: {
      email: "blocked-admin@example.invalid", role: "admin", oidc_subject: unboundSubject,
    } },
    { path: `/users/${target}/oidc`, method: "PUT", body: { oidc_subject: unboundSubject } },
    { path: `/users/${user}/oidc`, method: "PUT", body: { oidc_subject: unboundSubject } },
  ]) {
    for (const test of cases) {
      const before = (await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action IN ('user_created','user_oidc_linked')`)).trim();
      let afterChange;
      try {
        const status = await requestAfterChange(url, operation.path, { ...operation, token }, async () => {
          await test.change();
          afterChange = (await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action IN ('user_created','user_oidc_linked')`)).trim();
        });
        assert.equal(status, test.status, `${operation.method} ${operation.path}: ${test.name}`);
        assert.equal((await sql("SELECT count(*) FROM users WHERE email='blocked-admin@example.invalid'")).trim(), "0");
        assert.equal((await sql(`SELECT oidc_subject FROM users WHERE id='${target}'`)).trim(), "fixture-identity-target");
        assert.equal((await sql(`SELECT oidc_subject FROM users WHERE id='${user}'`)).trim(), subject);
        assert.equal((await sql(`SELECT count(*) FROM audit_logs WHERE tenant_id='${tenant}' AND action IN ('user_created','user_oidc_linked')`)).trim(), afterChange || before);
      } finally {
        await test.restore?.();
      }
      token = await login();
    }
  }
  const other = await api("/tokens", { method: "POST", token, body: { user_id: target, scopes: ["admin"], ttl_secs: 60 } });
  assert.equal(other.status, 201);
  const concurrent = await Promise.all([
    api(`/users/${target}/oidc`, { method: "PUT", token, body: { oidc_subject: "fixture-identity-target" } }),
    api(`/users/${user}/oidc`, { method: "PUT", token: other.data.token, body: { oidc_subject: subject } }),
  ]);
  assert.deepEqual(concurrent.map((r) => r.status).sort(), [204, 401], "opposite links serialize without deadlock and invalidate the waiting actor");
  token = await login();

  const service = randomBytes(32).toString("hex");
  const serviceHash = createHash("sha256").update(service).digest("hex");
  await sql(`INSERT INTO api_tokens(id,tenant_id,token_hash,scopes,expires_at,auth_method,user_auth_version)
    VALUES ('fixture-identity-service','${tenant}','${serviceHash}',ARRAY['admin'],now()+interval '1 minute','api',0)`);
  const serviceUser = await api(`/tenants/${tenant}/users`, { method: "POST", token: service,
    body: { email: "service-created@example.invalid", role: "member", oidc_subject: "fixture-service-created" } });
  assert.equal(serviceUser.status, 201, "a valid service actor needs no caller user lock");
  assert.equal((await api(`/users/${serviceUser.data.user_id}/oidc`, {
    method: "PUT", token: service, body: { oidc_subject: "fixture-service-linked" },
  })).status, 204);
  assert.equal(await requestAfterChange(url, `/users/${serviceUser.data.user_id}/oidc`, {
    method: "PUT", token: service, body: { oidc_subject: unboundSubject },
  }, () => mutate("/tokens/fixture-identity-service", "DELETE")), 401);
  console.log("PASS identity creation and linking reject revoked, disabled, expired or demoted actors after admission; no writes/audits, cross-user locks and service actors");
  return token;
}
