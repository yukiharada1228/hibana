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
