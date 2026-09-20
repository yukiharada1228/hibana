// Publishing requires successful branch checks for the exact release commit.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

export function requireReleaseChecks(runs, sha) {
  assert.match(sha, /^[a-f0-9]{40}$/);
  for (const path of [
    ".github/workflows/ci.yml",
    ".github/workflows/security.yml",
  ]) {
    const latest = runs
      .filter(
        (run) =>
          run.head_sha === sha &&
          run.path === path &&
          ["push", "workflow_dispatch", "schedule"].includes(run.event),
      )
      .sort((a, b) => b.id - a.id)[0];
    assert.ok(
      latest &&
        latest.status === "completed" &&
        latest.conclusion === "success",
      `${path} must succeed for ${sha}; latest result: ${latest?.conclusion || latest?.status || "missing"}`,
    );
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const repo = process.env.GITHUB_REPOSITORY;
  const sha = process.env.GITHUB_SHA;
  assert.match(repo || "", /^[\w.-]+\/[\w.-]+$/);
  assert.match(sha || "", /^[a-f0-9]{40}$/);
  const pages = JSON.parse(
    execFileSync(
      "gh",
      [
        "api",
        "--paginate",
        "--slurp",
        `repos/${repo}/actions/runs?head_sha=${sha}&per_page=100`,
      ],
      { encoding: "utf8", timeout: 30000, maxBuffer: 4 * 1024 * 1024 },
    ),
  );
  requireReleaseChecks(
    pages.flatMap((page) => page.workflow_runs),
    sha,
  );
  console.log(`Release gate: CI and dependency audit succeeded for ${sha}`);
}
