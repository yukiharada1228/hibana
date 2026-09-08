// Exercise the actual subprocess entry point, including its Linux memory limit.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

const executable = resolve(process.env.HIBANA_CONTROL_PLANE_BIN || "target/release/hibana-control-plane");
const component = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);
for (const threads of ["64", "0"]) {
  for (const valid of [true, false]) {
    const result = spawnSync(executable, ["--validate-stdin"], {
      // No server credentials are needed. Even an invalid Tokio setting must not
      // affect this synchronous, single-threaded validation process.
      env: { ...process.env, TOKIO_WORKER_THREADS: threads, VALIDATION_MEM_LIMIT_MB: "256" },
      input: valid ? component : Buffer.from("invalid wasm"),
      encoding: "utf8", timeout: 5000, maxBuffer: 65536,
    });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr);
    const outcome = JSON.parse(result.stdout);
    if (valid) assert.equal(outcome.Ok.size_bytes, component.length);
    else assert.equal(typeof outcome.Rejected.message, "string");
  }
}
console.log("PASS validation subprocess: 256 MiB limit, valid/rejected input, independent of Tokio settings");
