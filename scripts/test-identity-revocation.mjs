import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { holdTransaction, waitForBlocked } from "./oidc-test-transaction.mjs";

export async function testIdentityRevocation({ api, sql, pg, tenant, user, token, login }) {
  const createUser = async (name, role = "admin") => {
    const r = await api(`/tenants/${tenant}/users`, { method: "POST", token,
      body: { email: `${name}@example.invalid`, role, oidc_subject: `fixture-${name}` } });
    assert.equal(r.status, 201);
    return r.data.user_id;
  };
  const mint = async (user_id, scopes = ["read", "admin"]) => {
    const r = await api("/tokens", { method: "POST", token, body: { user_id, scopes, ttl_secs: 300 } });
    assert.equal(r.status, 201);
    return r.data.token;
  };
  const target = await createUser("revocation-target");
  const targetToken = await mint(target);
  const auditCount = async () => (await sql(`SELECT count(*) FROM audit_logs WHERE target='${target}' AND action IN ('user_disabled','user_tokens_revoked')`)).trim();
  const snapshot = (await sql(`SELECT auth_version||'|'||(deleted_at IS NULL) FROM users WHERE id='${target}'`)).trim();
  const changes = [
    { name: "token revoked", token: "revoked_at=now()", status: 401 },
    { name: "token expired", token: "expires_at=now()-interval '1 second'", status: 401 },
    { name: "generation changed", user: "auth_version=auth_version+1", status: 401 },
    { name: "caller disabled", user: "deleted_at=now(),auth_version=auth_version+1", status: 401 },
    { name: "role demoted", user: "role='member'", status: 403 },
    { name: "scope removed", token: "scopes=ARRAY['read']", status: 403 },
  ];
  for (const [method, path] of [["DELETE", `/users/${target}`], ["POST", `/users/${target}/revoke-tokens`]]) {
    for (const change of changes) {
      const hash = createHash("sha256").update(token).digest("hex");
      const before = await auditCount();
      // Hold both identities before the handler acquires its ordered locks, so
      // changes to either actor metadata or token can commit first, without sleeps.
      const finish = await holdTransaction(pg, `SELECT id FROM users WHERE id IN ('${user}','${target}') ORDER BY id FOR UPDATE;`);
      const pending = api(path, { method, token });
      pending.catch(() => {});
      try {
        await waitForBlocked(sql);
        await finish(change.user
          ? `UPDATE users SET ${change.user} WHERE id='${user}';`
          : `UPDATE api_tokens SET ${change.token} WHERE token_hash='${hash}';`);
        assert.equal((await pending).status, change.status, `${method} ${change.name}`);
        assert.equal(await auditCount(), before, "denied revocation writes no success audit");
        assert.equal((await sql(`SELECT auth_version||'|'||(deleted_at IS NULL) FROM users WHERE id='${target}'`)).trim(), snapshot);
        assert.equal((await api("/auth/session", { token: targetToken })).status, 200);
      } finally {
        await finish();
        await pending;
        await sql(`UPDATE users SET deleted_at=NULL,role='admin' WHERE id='${user}'`);
      }
      token = await login();
    }
  }
  // The original reproduction: only the target is locked; a real logout of the
  // caller completes while its management request is waiting.
  const finish = await holdTransaction(pg, `SELECT id FROM users WHERE id='${target}' FOR UPDATE;`);
  const pending = api(`/users/${target}`, { method: "DELETE", token });
  pending.catch(() => {});
  try {
    await waitForBlocked(sql);
    assert.equal((await api("/auth/logout", { method: "POST", token })).status, 204);
  } finally { await finish(); }
  assert.equal((await pending).status, 401);
  token = await login();

  const service = randomBytes(32).toString("hex");
  const serviceHash = createHash("sha256").update(service).digest("hex");
  await sql(`INSERT INTO api_tokens(id,tenant_id,token_hash,scopes,expires_at,auth_method,user_auth_version)
    VALUES ('fixture-revocation-service','${tenant}','${serviceHash}',ARRAY['admin'],now()+interval '5 minutes','api',0)`);
  const serviceLock = await holdTransaction(pg, `SELECT id FROM users WHERE id='${target}' FOR UPDATE;`);
  const servicePending = api(`/users/${target}/revoke-tokens`, { method: "POST", token: service });
  servicePending.catch(() => {});
  try {
    await waitForBlocked(sql);
    assert.equal((await api("/auth/logout", { method: "POST", token: service })).status, 204);
  } finally { await serviceLock(); }
  assert.equal((await servicePending).status, 401);
  await sql("UPDATE api_tokens SET revoked_at=NULL WHERE id='fixture-revocation-service'");
  assert.equal((await api(`/users/${target}/revoke-tokens`, { method: "POST", token: service })).status, 204);
  assert.equal((await api("/auth/session", { token: targetToken })).status, 401);

  const member = await createUser("revocation-member", "member");
  const memberToken = await mint(member, ["read"]);
  const sibling = await mint(member, ["read"]);
  const memberLock = await holdTransaction(pg, `SELECT id FROM users WHERE id='${member}' FOR UPDATE;`);
  const memberPending = api("/auth/logout-all", { method: "POST", token: memberToken });
  memberPending.catch(() => {});
  try {
    await waitForBlocked(sql);
    assert.equal((await api("/auth/logout", { method: "POST", token: memberToken })).status, 204);
  } finally { await memberLock(); }
  assert.equal((await memberPending).status, 401);
  assert.equal((await api("/auth/session", { token: sibling })).status, 200);
  assert.equal((await api("/auth/logout-all", { method: "POST", token: sibling })).status, 204, "members may still log out all their sessions");
  assert.equal((await api("/auth/session", { token: sibling })).status, 401);

  const otherToken = await mint(target);
  const concurrent = await Promise.all([
    api(`/users/${target}/revoke-tokens`, { method: "POST", token }),
    api(`/users/${user}/revoke-tokens`, { method: "POST", token: otherToken }),
  ]);
  assert.deepEqual(concurrent.map((r) => r.status).sort(), [204, 401], "opposite revocations serialize without deadlock");
  token = await login();
  assert.equal((await api(`/users/${target}`, { method: "DELETE", token })).status, 204);
  console.log("PASS user revocation/disable revalidate callers after lock waits; denied writes leave target/audits unchanged; service actors, member logout-all and opposite revocations work");
  return token;
}
