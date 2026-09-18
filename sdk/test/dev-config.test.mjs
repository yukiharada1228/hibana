import test from "node:test";
import assert from "node:assert/strict";
import { devConfig } from "../src/dev-config.mjs";

test("local destinations are explicit, bounded and deduplicated by host and port", () => {
  assert.deepEqual(devConfig(undefined), { allow_outbound: [] });
  assert.deepEqual(devConfig({}), { allow_outbound: [] });
  assert.deepEqual(devConfig({ allow_outbound: [" DB.Example.COM.:05432 ", "db.example.com:5432", "db.example.com:5433", "[2606:4700::1111]:443"] }), {
    allow_outbound: ["db.example.com:5432", "db.example.com:5433", "[2606:4700::1111]:443"],
  });
  for (const invalid of [null, [], true, { enabled: true }, { allow_outbound: null }, { allow_outbound: "db:5432" }, { allow_outbound: Array(65).fill("db:5432") }])
    assert.throws(() => devConfig(invalid), /dev/);
});

test("local destinations reject URLs, credentials, wildcards and invalid ports without echoing input", () => {
  for (const entry of [null, 42, "", "db", "db:0", "db:65536", "db:-1", "db:1.5", "db:443/path", "https://db:443", "user:sensitive-value@db:5432", "*.example:443", "a..b:443", "-db:5432", "db-:5432", "[host]:443", "[::g]:443", "::1:443", "db\n:5432", "db\0:5432", `${"x".repeat(64)}:5432`]) {
    assert.throws(() => devConfig({ allow_outbound: [entry] }), error => {
      assert.match(error.message, /HOST:PORT/);
      assert.doesNotMatch(error.message, /sensitive-value/);
      return true;
    });
  }
});
