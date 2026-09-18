import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { apiClient, ApiError, deploy } from "../src/api.mjs";

const config = { name: "first-deploy", vars: {}, secrets: [], resources: {} };
const component = { component_id: "cmp_fixture", name: config.name };
const json = (value, status = 200) =>
  new Response(JSON.stringify(value), { status });

async function fixture(t, respond) {
  const home = await mkdtemp(join(tmpdir(), "hibana-api-"));
  const previous = {
    HIBANA_CONFIG_HOME: process.env.HIBANA_CONFIG_HOME,
    HIBANA_PROFILE: process.env.HIBANA_PROFILE,
  };
  process.env.HIBANA_CONFIG_HOME = home;
  delete process.env.HIBANA_PROFILE;
  t.after(async () => {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    await rm(home, { recursive: true, force: true });
  });
  const calls = [];
  t.mock.method(globalThis, "fetch", async (url, options) => {
    const call = { path: new URL(url).pathname, ...options };
    calls.push(call);
    return respond(call, calls.length);
  });
  const artifact = join(home, "app.wasm");
  await writeFile(artifact, Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]));
  const api = await apiClient({
    url: "https://fixture.invalid",
    token: "test-only",
  });
  return { api, artifact, calls };
}

test("concurrent first deployments reuse the winning component and upload both versions once", async (t) => {
  let created = false,
    reads = 0;
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const { api, artifact, calls } = await fixture(
    t,
    async ({ path, method, body }) => {
      if (path === "/components" && method === "GET") {
        const response = json(created ? [component] : []);
        // Both callers observe absence before either starts creation.
        if (++reads <= 2) {
          if (reads === 2) release();
          await gate;
        }
        return response;
      }
      if (path === "/components") {
        assert.deepEqual(JSON.parse(body), { name: config.name });
        if (created) return json({ error: { code: "conflict" } }, 409);
        created = true;
        return json(component, 201);
      }
      assert.equal(path, "/components/cmp_fixture/versions");
      return json({ version_id: `ver_${body.get("version")}` }, 201);
    },
  );
  const results = await Promise.all([
    deploy(api, config, artifact, "first-a"),
    deploy(api, config, artifact, "first-b"),
  ]);
  assert.deepEqual(
    results.map((value) => value.component_id),
    [component.component_id, component.component_id],
  );
  assert.deepEqual(
    results.map((value) => value.version_id),
    ["ver_first-a", "ver_first-b"],
  );
  assert.equal(
    calls.filter(
      (call) => call.path === "/components" && call.method === "POST",
    ).length,
    2,
  );
  assert.equal(reads, 3, "the losing create performs one fresh lookup");
  assert.equal(
    calls.filter((call) => call.path.endsWith("/versions")).length,
    2,
  );
});

for (const status of [400, 401, 403, 429, 500]) {
  test(`component creation HTTP ${status} is not retried and its body remains private`, async (t) => {
    const { api, artifact, calls } = await fixture(t, ({ method }) =>
      method === "GET"
        ? json([])
        : json({ error: { message: "private-server-value" } }, status),
    );
    await assert.rejects(deploy(api, config, artifact, "first"), (error) => {
      assert.ok(error instanceof ApiError);
      assert.equal(error.status, status);
      assert.equal(error.message, `POST /components: HTTP ${status}`);
      assert.doesNotMatch(JSON.stringify(error), /private-server-value/);
      return true;
    });
    assert.deepEqual(
      calls.map((call) => call.method),
      ["GET", "POST"],
    );
  });
}

test("a transport failure during creation is not retried", async (t) => {
  const failure = new TypeError("fetch failed");
  const { api, artifact, calls } = await fixture(t, ({ method }) => {
    if (method === "GET") return json([]);
    throw failure;
  });
  await assert.rejects(
    deploy(api, config, artifact, "first"),
    (error) => error === failure,
  );
  assert.equal(calls.length, 2);
});

test("a conflict with no matching live component stops after one lookup", async (t) => {
  const { api, artifact, calls } = await fixture(t, ({ method }) =>
    method === "GET" ? json([{ ...component, name: "other" }]) : json({}, 409),
  );
  await assert.rejects(deploy(api, config, artifact, "first"), {
    message: "POST /components: HTTP 409",
  });
  assert.deepEqual(
    calls.map((call) => call.method),
    ["GET", "POST", "GET"],
  );
});

test("an upload conflict does not trigger component lookup or upload retry", async (t) => {
  const { api, artifact, calls } = await fixture(t, ({ method }) =>
    method === "GET" ? json([component]) : json({}, 409),
  );
  await assert.rejects(deploy(api, config, artifact, "first"), {
    message: "POST /components/cmp_fixture/versions: HTTP 409",
  });
  assert.deepEqual(
    calls.map((call) => call.path),
    ["/components", "/components/cmp_fixture/versions"],
  );
});
