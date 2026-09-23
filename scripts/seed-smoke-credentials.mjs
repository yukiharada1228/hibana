// GitHub Actions' disposable Compose stack only. Never part of the application.
import assert from "node:assert/strict";
import { appendFile } from "node:fs/promises";
import { runCommand } from "./bounded-process.mjs";
import { issueFixtureToken } from "./test-api-credentials.mjs";

assert.equal(process.env.CI, "true");
assert.ok(process.env.GITHUB_ENV);
const sql = (query) => runCommand("docker", [
  "compose", "exec", "-T", "postgres", "psql", "-XqAt", "-U", "faas", "-d", "faas",
  "-v", "ON_ERROR_STOP=1", "-c", query,
]);
const { token } = await (await issueFixtureToken(sql, {
  tenant_slug: "smoke", email: "admin@example.com",
})).json();
console.log(`::add-mask::${token}`);
await appendFile(process.env.GITHUB_ENV, `HIBANA_TOKEN=${token}\n`);
