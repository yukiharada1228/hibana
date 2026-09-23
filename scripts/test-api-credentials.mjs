// Test setup only: call with the disposable database's owner SQL helper.
// Application tests use API credentials; real OIDC login is tested separately.
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";

const literal = (value) => "'" + String(value).replaceAll("'", "''") + "'";

export async function issueFixtureToken(sql, {
  tenant_slug, email, scopes = ["read", "deploy", "admin"], ttl_secs = 3600,
}) {
  assert.ok(scopes.every((s) => ["read", "deploy", "admin"].includes(s)));
  assert.ok(Number.isInteger(ttl_secs) && ttl_secs >= 60 && ttl_secs <= 90000,
    "fixture token lifetime must be 60..90000 seconds");
  const token = randomBytes(32).toString("hex");
  const token_id = `fixture-${randomBytes(16).toString("hex")}`;
  const hash = createHash("sha256").update(token).digest("hex");
  const result = await sql(`INSERT INTO api_tokens
    (id,tenant_id,user_id,token_hash,scopes,expires_at,auth_method,user_auth_version)
    SELECT '${token_id}',u.tenant_id,u.id,'${hash}',ARRAY[${scopes.map(literal)}]::text[],
      now()+${ttl_secs}*interval '1 second','api',u.auth_version
    FROM users u JOIN tenants t ON u.tenant_id=t.id
    WHERE t.slug=${literal(tenant_slug)} AND u.email=${literal(email)}
      AND u.deleted_at IS NULL AND t.status='active'
    RETURNING id`);
  assert.equal(result.trim(), token_id, "fixture must resolve exactly one active user");
  return Response.json({ token, token_id, scopes, expires_at: new Date(Date.now() + ttl_secs * 1000).toISOString() }, { status: 201 });
}
