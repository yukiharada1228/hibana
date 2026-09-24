import assert from "node:assert/strict";
import test from "node:test";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import { requireReleaseChecks } from "./check-release-ci.mjs";

const sha = "a".repeat(40);

test("npm verification retries installation lag, bounds failures and rejects a wrong version", () => {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const workflow = readFileSync(join(root, ".github/workflows/release.yml"), "utf8");
  const blocks = [...workflow.matchAll(/      - name: Verify npx against the public registry\n        run: \|\n((?:          .*\n)+)/g)]
    .map(match => match[1].replace(/^          /gm, ""));
  assert.equal(blocks.length, 2);
  assert.equal(blocks[0], blocks[1], "publish and resume must verify installation the same way");
  assert.match(workflow, /name: Publish the verified tarball using npm trusted publishing\n        if: \$\{\{ !inputs\.verify_only \}\}/);
  const directory = mkdtempSync(join(tmpdir(), "hibana-npm-verification-"));
  const version = execFileSync(process.execPath, ["scripts/release.mjs", "version"], { cwd: root, encoding: "utf8" }).trim();
  try {
    writeFileSync(join(directory, "npx"), `#!${process.execPath}
const fs = require('node:fs');
const count = Number(fs.readFileSync(process.env.HIBANA_TEST_ATTEMPTS, 'utf8')) + 1;
fs.writeFileSync(process.env.HIBANA_TEST_ATTEMPTS, String(count));
if (!process.argv.includes('--prefer-online')) process.exit(2);
if (process.env.HIBANA_TEST_MODE === 'unavailable' || process.env.HIBANA_TEST_MODE === 'lag' && count === 1) process.exit(1);
console.log(process.env.HIBANA_TEST_MODE === 'wrong' ? 'hibana 0.0.0' : 'hibana ' + process.env.HIBANA_TEST_VERSION);
`, { mode: 0o700 });
    writeFileSync(join(directory, "sleep"), "#!/bin/sh\nexit 0\n", { mode: 0o700 });
    for (const [mode, expectedAttempts, success] of [["lag", 2, true], ["unavailable", 45, false], ["wrong", 1, false]]) {
      const attempts = join(directory, "attempts");
      writeFileSync(attempts, "0");
      const result = spawnSync("bash", ["-e", "-o", "pipefail", "-c", blocks[0]], {
        cwd: root, encoding: "utf8", timeout: 15000,
        env: { ...process.env, PATH: directory + delimiter + process.env.PATH,
          HIBANA_TEST_ATTEMPTS: attempts, HIBANA_TEST_MODE: mode, HIBANA_TEST_VERSION: version },
      });
      assert.equal(result.error, undefined);
      assert.equal(result.status === 0, success, result.stderr);
      assert.equal(Number(readFileSync(attempts, "utf8")), expectedAttempts);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
const successful = ["ci", "security"].map((name, index) => ({
  id: index + 1,
  path: `.github/workflows/${name}.yml`,
  head_sha: sha,
  event: "push",
  status: "completed",
  conclusion: "success",
  updated_at: "2026-09-20T10:00:00Z",
}));

test("exact release commit requires both successful branch checks", () => {
  requireReleaseChecks(successful, sha);
  assert.throws(() => requireReleaseChecks(successful.slice(0, 1), sha));
  assert.throws(() => requireReleaseChecks(successful, "b".repeat(40)));
  assert.throws(() =>
    requireReleaseChecks(
      successful.map((run) => ({ ...run, event: "pull_request" })),
      sha,
    ),
  );
});

test("a rerun of an older run is ordered by its latest result, not its ID", () => {
  for (const run of successful) {
    const olderRerun = {
      ...run,
      id: 0,
      updated_at: "2026-09-20T11:00:00Z",
      conclusion: "failure",
    };
    assert.throws(() => requireReleaseChecks([...successful, olderRerun], sha));
    requireReleaseChecks(
      [...successful, { ...olderRerun, conclusion: "success" }],
      sha,
    );
    assert.throws(() =>
      requireReleaseChecks(
        [...successful, { ...olderRerun, updated_at: run.updated_at }],
        sha,
      ),
    );
    assert.throws(() =>
      requireReleaseChecks(
        [
          ...successful,
          {
            ...olderRerun,
            updated_at: "2026-09-20T09:00:00Z",
            status: "queued",
          },
        ],
        sha,
      ),
    );
    requireReleaseChecks(
      [...successful, { ...olderRerun, updated_at: "2026-09-20T09:00:00Z" }],
      sha,
    );
    assert.throws(() =>
      requireReleaseChecks(
        [...successful, { ...olderRerun, updated_at: undefined }],
        sha,
      ),
    );
  }
});

test("older success cannot mask a failed, canceled or pending rerun", () => {
  for (const [status, conclusion] of [
    ["completed", "failure"],
    ["completed", "cancelled"],
    ["completed", "skipped"],
    ["in_progress", null],
    ["queued", null],
  ]) {
    for (const run of successful) {
      assert.throws(() =>
        requireReleaseChecks(
          [...successful, { ...run, id: 10, status, conclusion }],
          sha,
        ),
      );
    }
  }
  assert.throws(
    () =>
      requireReleaseChecks(
        [
          ...successful,
          {
            ...successful[1],
            id: 30,
            event: "schedule",
            conclusion: "failure",
          },
        ],
        sha,
      ),
    "a newer scheduled security finding must also block publication",
  );
});

test("unrelated workflows and commits do not substitute for required checks", () => {
  assert.throws(() =>
    requireReleaseChecks(
      successful.map((run) => ({ ...run, path: "other.yml" })),
      sha,
    ),
  );
  requireReleaseChecks(
    [
      ...successful,
      {
        ...successful[0],
        id: 20,
        head_sha: "b".repeat(40),
        conclusion: "failure",
      },
    ],
    sha,
  );
});
