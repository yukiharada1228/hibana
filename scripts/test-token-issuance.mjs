import assert from "node:assert/strict";
import { request } from "node:http";
import { createHash, randomBytes } from "node:crypto";

// Hyper sends 100 Continue when the JSON extractor first polls the body, after
// authentication. This avoids timing guesses about whether middleware ran.
export async function requestAfterChange(url, path, { method, token, body }, change) {
  const payload = JSON.stringify(body);
  let admitted, admissionFailed, completed, failed;
  const admission = new Promise((resolve, reject) => {
    admitted = resolve; admissionFailed = reject;
  });
  const result = new Promise((resolve, reject) => {
    completed = resolve; failed = reject;
  });
  result.catch(() => {});
  const pending = request(url + path, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
      "Content-Length": Buffer.byteLength(payload),
      Expect: "100-continue",
    },
    timeout: 5000,
  }, (response) => {
    admissionFailed(new Error("Request completed before body admission"));
    response.resume();
    response.once("end", () => completed(response.statusCode));
    response.once("error", failed);
  });
  pending.once("continue", admitted);
  pending.once("error", (error) => { admissionFailed(error); failed(error); });
  pending.once("timeout", () => pending.destroy(new Error("Request timed out")));
  try {
    pending.flushHeaders();
    await admission;
    await change();
    pending.end(payload);
    return await result;
  } finally {
    pending.destroy();
  }
}

const mintAfterChange = (url, token, user, change) => requestAfterChange(url, "/tokens", {
  method: "POST", token,
  body: { user_id: user, scopes: ["read"], name: "revocation-race", ttl_secs: 60 },
}, change);

export async function testTokenIssuance({ api, sql, url, tenant, user, subject, token, login }) {
  const created = await api(`/tenants/${tenant}/users`, {
    method: "POST", token,
    body: { email: "token-target@example.invalid", role: "admin", oidc_subject: "fixture-token-target" },
  });
  assert.equal(created.status, 201);
  const other = created.data.user_id;
  const mutate = async (path, method = "POST", body) => {
    const response = await api(path, { method, token, body });
    assert.equal(response.status, 204);
  };
  const restoreRole = () => sql(`UPDATE users SET role='admin' WHERE id='${user}'`);
  const callerHash = () => createHash("sha256").update(token).digest("hex");
  const restoreScopes = () => sql(`UPDATE api_tokens SET scopes=ARRAY['read','invoke','deploy','admin'] WHERE token_hash='${callerHash()}'`);
  const cases = [
    { name: "logout-all / same user", target: user, change: () => mutate("/auth/logout-all"), status: 401 },
    { name: "logout-all / other user", target: other, change: () => mutate("/auth/logout-all"), status: 401 },
    { name: "admin revocation", target: other, change: () => mutate(`/users/${user}/revoke-tokens`), status: 401 },
    { name: "single-token logout", target: user, change: () => mutate("/auth/logout"), status: 401 },
    { name: "identity relink", target: other, change: () => mutate(`/users/${user}/oidc`, "PUT", { oidc_subject: subject }), status: 401 },
    { name: "expired caller", target: user, change: () => sql(`UPDATE api_tokens SET expires_at=now()-interval '1 second' WHERE token_hash='${callerHash()}'`), status: 401 },
    { name: "role downgrade", target: other, change: () => sql(`UPDATE users SET role='member' WHERE id='${user}'`), restore: restoreRole, status: 403 },
    { name: "admin scope removal", target: other, change: () => sql(`UPDATE api_tokens SET scopes=ARRAY['read'] WHERE token_hash='${callerHash()}'`), restore: restoreScopes, status: 403 },
  ];
  for (const test of cases) {
    try {
      assert.equal(await mintAfterChange(url, token, test.target, test.change), test.status, test.name);
      assert.equal((await sql("SELECT count(*) FROM api_tokens WHERE name='revocation-race'")).trim(), "0", test.name);
    } finally {
      await test.restore?.();
    }
    token = await login();
  }

  const otherLogin = await api("/tokens", {
    method: "POST", token,
    body: { user_id: other, scopes: ["read", "admin"], ttl_secs: 60 },
  });
  assert.equal(otherLogin.status, 201);
  // Opposite owner/target orders must acquire user locks in the same order.
  const concurrent = await Promise.all([
    api("/tokens", { method: "POST", token, body: { user_id: other, scopes: ["read"], ttl_secs: 60 } }),
    api("/tokens", { method: "POST", token: otherLogin.data.token, body: { user_id: user, scopes: ["read"], ttl_secs: 60 } }),
  ]);
  assert.deepEqual(concurrent.map((response) => response.status), [201, 201]);
  assert.equal(await mintAfterChange(url, otherLogin.data.token, user,
    () => mutate(`/users/${other}`, "DELETE")), 401, "disabled caller cannot issue for an active target");
  assert.equal((await sql("SELECT count(*) FROM api_tokens WHERE name='revocation-race'")).trim(), "0");

  const serviceSecret = randomBytes(32).toString("hex");
  const serviceHash = createHash("sha256").update(serviceSecret).digest("hex");
  await sql(`INSERT INTO api_tokens(id,tenant_id,token_hash,scopes,name,expires_at,auth_method,user_auth_version)
    VALUES ('fixture-service-issuer','${tenant}','${serviceHash}',ARRAY['read','admin'],'service',now()+interval '1 minute','api',0)`);
  const service = await api("/tokens", {
    method: "POST", token: serviceSecret,
    body: { user_id: user, scopes: ["read"], name: "login", ttl_secs: 60 },
  });
  assert.equal(service.status, 201, "valid service issuers still work");
  assert.equal((await api("/auth/session", { token: service.data.token })).status, 200);
  assert.equal(await mintAfterChange(url, serviceSecret, user,
    () => mutate("/tokens/fixture-service-issuer", "DELETE")), 401);
  console.log("PASS token issuance rechecks revocation, generation, expiry, role and scope after body admission; cross-user locks and service tokens");
  return token;
}
