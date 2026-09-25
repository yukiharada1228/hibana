// Publishing requires successful branch checks for the exact release commit.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";
import { setTimeout } from "node:timers/promises";

const workflows = [
  ".github/workflows/ci.yml",
  ".github/workflows/security.yml",
  ".github/workflows/infra.yml",
];

function applicable(runs, sha, path) {
  return runs.filter(
    (run) => run.head_sha === sha && run.path === path &&
      ["push", "workflow_dispatch", "schedule"].includes(run.event),
  );
}

export function releaseChecksPending(runs, sha) {
  return workflows.some((path) => {
    const matching = applicable(runs, sha, path);
    return !matching.length || matching.some((run) => run.status !== "completed");
  });
}

export function requireReleaseChecks(runs, sha) {
  assert.match(sha, /^[a-f0-9]{40}$/);
  for (const path of workflows) {
    const matching = applicable(runs, sha, path);
    assert.ok(matching.length, `${path} must run for ${sha}`);
    // A rerun keeps its original ID. Compare result updates, and never publish
    // while another applicable check is still running (including an old rerun).
    for (const run of matching) {
      assert.equal(run.status, "completed", `${path} is still ${run.status}`);
      assert.ok(
        Number.isFinite(Date.parse(run.updated_at)),
        `${path} has no result timestamp`,
      );
    }
    const latestTime = Math.max(
      ...matching.map((run) => Date.parse(run.updated_at)),
    );
    const latest = matching.filter(
      (run) => Date.parse(run.updated_at) === latestTime,
    );
    assert.ok(
      latest.every((run) => run.conclusion === "success"),
      `${path} must succeed for ${sha}; latest result: ${latest.map((run) => run.conclusion).join(", ")}`,
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
  const deadline =
    Date.now() + (process.argv.includes("--wait") ? 30 * 60 * 1000 : 0);
  for (;;) {
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
    const runs = pages.flatMap((page) => page.workflow_runs);
    if (releaseChecksPending(runs, sha) && Date.now() < deadline) {
      console.log(`Waiting for exact-commit CI, security and VPS IaC: ${sha}`);
      await setTimeout(20000);
      continue;
    }
    requireReleaseChecks(runs, sha);
    break;
  }
  console.log(`Release gate: CI, dependency audit and VPS IaC succeeded for ${sha}`);
}
