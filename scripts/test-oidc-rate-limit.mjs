import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { runCommand } from "./bounded-process.mjs";

export async function testOidcRateLimit({ base, secondary, consoleUrl, redis }) {
  const command = (...args) =>
    runCommand("docker", ["exec", redis, "redis-cli", "--raw", ...args]);
  const globalKey = "rl:oidc:global";
  const blockedIp = "198.51.100.1";
  const healthyIp = "198.51.100.2";
  const keys = [globalKey, `rl:oidc:ip:${blockedIp}`, `rl:oidc:ip:${healthyIp}`];
  // A future timestamp prevents refill on the next check. Reseed before each
  // request so assertions do not depend on how quickly the CI host runs.
  const seed = (key, tokens) =>
    command(
      "HSET", key, "tokens", String(tokens),
      "last", String(Date.now() + 3_600_000),
    );
  const start = async (endpoint, ip) => {
    const response = await fetch(`${endpoint}/auth/oidc/start`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-forwarded-for": ip },
      body: JSON.stringify({
        tenant_slug: "team",
        redirect_uri: consoleUrl,
        state: randomBytes(32).toString("base64url"),
        code_challenge: createHash("sha256")
          .update("rate-limit-fixture")
          .digest("base64url"),
      }),
      signal: AbortSignal.timeout(10_000),
    });
    await response.arrayBuffer();
    return response.status;
  };

  await command("DEL", ...keys);
  try {
    await seed(globalKey, 30);
    const before = await command("HMGET", globalKey, "tokens", "last");
    for (const endpoint of [base, secondary]) {
      await seed(`rl:oidc:ip:${blockedIp}`, 0);
      assert.equal(
        await start(endpoint, blockedIp), 503,
        "depleted IP is rejected on either replica",
      );
      assert.equal(
        await command("HMGET", globalKey, "tokens", "last"), before,
        "IP rejection must leave the shared allowance untouched",
      );
    }
    assert.equal(
      await start(secondary, healthyIp), 200,
      "another IP can still start login",
    );
    assert.equal(
      Number(await command("HGET", globalKey, "tokens")), 29,
      "an admitted IP still consumes the shared allowance",
    );
    await seed(globalKey, 0);
    assert.equal(
      await start(base, healthyIp), 503,
      "shared exhaustion still rejects an admitted IP",
    );
    console.log("PASS OIDC IP rejection preserves the shared allowance across replicas; other IPs can log in; global limiting remains enforced");
  } finally {
    await command("DEL", ...keys);
  }
}
