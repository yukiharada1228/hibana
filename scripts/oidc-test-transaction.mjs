// Bounded, disposable PostgreSQL transactions for deterministic OIDC races.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

export async function holdTransaction(pg, statements) {
  assert.match(pg, /^hibana-oidc-pg-[0-9]+$/);
  const child = spawn("docker", ["exec", "-i", pg, "psql", "-XqAt", "-U", "postgres", "-d", "hibana_oidc", "-v", "ON_ERROR_STOP=1"], { stdio: ["pipe", "pipe", "pipe"] });
  child.stderr.resume();
  child.stdin.on("error", () => {});
  const exited = new Promise((resolve) => child.once("close", resolve));
  const watchdog = setTimeout(() => child.kill("SIGKILL"), 15_000);
  const finish = async (statements = "") => {
    if (!child.stdin.writableEnded) child.stdin.end(`${statements}\nCOMMIT;\n`);
    const code = await exited;
    clearTimeout(watchdog);
    assert.equal(code, 0, "fixture transaction must commit");
  };
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("Fixture transaction deadline")), 5000);
      let output = "";
      child.stdout.on("data", (chunk) => {
        output += chunk;
        if (output.includes("FIXTURE_READY")) { clearTimeout(timer); resolve(); }
      });
      child.once("error", (error) => { clearTimeout(timer); reject(error); });
      child.once("close", () => { clearTimeout(timer); reject(new Error("Fixture transaction exited early")); });
      child.stdin.write(`BEGIN; SET LOCAL idle_in_transaction_session_timeout='12s'; SET LOCAL statement_timeout='10s'; ${statements}\nSELECT 'FIXTURE_READY';\n`);
    });
    return finish;
  } catch (error) {
    await finish("ROLLBACK;");
    throw error;
  }
}

export async function waitForBlocked(sql, count = 1) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    // Legacy INSERT fixtures connect as the owner and SET LOCAL ROLE faas_app;
    // pg_stat_activity.usename still reports the original connection role.
    const blocked = Number((await sql(`SELECT count(*) FROM pg_stat_activity
      WHERE datname=current_database() AND wait_event_type='Lock'
      AND cardinality(pg_blocking_pids(pid)) > 0`)).trim());
    if (blocked === count) return;
    await sleep(20);
  }
  assert.fail(`Expected ${count} operations waiting for the fixture transaction`);
}
