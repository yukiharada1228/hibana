import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { browserLogin } from "../src/oidc.mjs";

test("OIDC CLI binds loopback, checks state and protects token exchange with PKCE", async () => {
  let started, exchanged = 0;
  const api = { tenant: "team", request: async (path, options) => {
    assert.equal(options.auth, false);
    if (path === "/auth/oidc/start") {
      started = options.body;
      assert.equal(new URL(started.redirect_uri).hostname, "127.0.0.1");
      return { authorization_url: "https://id.example/authorize" };
    }
    assert.equal(path, "/auth/oidc/exchange");
    assert.equal(createHash("sha256").update(options.body.code_verifier).digest("base64url"), started.code_challenge);
    assert.equal(options.body.code, "c".repeat(64));
    exchanged++;
    return { token: "fixture-token" };
  }};
  const result = await browserLogin(api, { log: () => {}, open: async () => {
    const callback = new URL(started.redirect_uri);
    callback.searchParams.set("oidc_code", "c".repeat(64));
    callback.searchParams.set("oidc_state", "wrong-state");
    assert.equal((await fetch(callback)).status, 400);
    callback.searchParams.set("oidc_state", started.state);
    assert.equal((await fetch(callback, { headers: { Host: "untrusted.example" } })).status, 400);
    assert.equal((await fetch(callback)).status, 200);
  }});
  assert.equal(result.token, "fixture-token");
  assert.equal(exchanged, 1);
  await assert.rejects(fetch(started.redirect_uri));
});

test("OIDC CLI timeout and cancellation close the callback listener", async () => {
  for (const cancel of [false, true]) {
    let redirect;
    const controller = new AbortController();
    const api = { tenant: "team", request: async (_, options) => {
      redirect = options.body.redirect_uri;
      if (cancel) setTimeout(() => controller.abort(), 10);
      return { authorization_url: "https://id.example/authorize" };
    }};
    const before = process.listenerCount("SIGINT");
    await assert.rejects(browserLogin(api, { noBrowser: true, signal: controller.signal, log: () => {}, timeoutMs: 50 }), /cancelled|timed out/);
    assert.equal(process.listenerCount("SIGINT"), before);
    await assert.rejects(fetch(redirect));
  }
});

test("OIDC CLI refuses unsafe authorization URLs", async () => {
  for (const authorization_url of ["javascript:alert(1)", "http://id.example/authorize", "https://user:password@id.example/authorize"]) {
    let opened = false;
    await assert.rejects(browserLogin({ tenant: "team", request: async () => ({ authorization_url }) }, { log: () => {}, open: async () => { opened = true; } }), /HTTPS|Invalid/);
    assert.equal(opened, false);
  }
});
