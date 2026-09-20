import assert from "node:assert/strict";
import test from "node:test";
import { requireReleaseChecks } from "./check-release-ci.mjs";

const sha = "a".repeat(40);
const successful = ["ci", "security"].map((name, index) => ({
  id: index + 1,
  path: `.github/workflows/${name}.yml`,
  head_sha: sha,
  event: "push",
  status: "completed",
  conclusion: "success",
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
