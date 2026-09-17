// Real API/ORM regression; only run by test-http.sh in its disposable database.
import assert from "node:assert/strict";

export async function testApplicationEgress({
  api,
  sql,
  token,
  wasm,
  upload,
  holdStorage,
  releaseStorage,
}) {
  const create = async () => {
    const r = await api("/components", {
      token,
      method: "POST",
      body: { name: "egress-policy" },
    });
    assert.equal(r.status, 201);
    return (await r.json()).component_id;
  };
  const id = await create(),
    base = `/components/${id}`;
  const read = async (path, auth = token) => {
    const r = await api(path, { token: auth });
    assert.equal(r.status, 200);
    return r.json();
  };
  const change = async (body, auth = token) =>
    api(`${base}/egress`, { token: auth, method: "PATCH", body });
  const caps = (version) => read(`${base}/versions/${version}/capabilities`);
  const legacy = (allow_outbound) =>
    api(`${base}/versions/one/capabilities/egress`, {
      token,
      method: "PUT",
      body: { allow_outbound },
    });
  assert.deepEqual(await read(`${base}/egress`), { allow_outbound: null });
  assert.equal(
    (await upload(id, token, "one", wasm, 0, { vars: { GREETING: "keep" } }))
      .status,
    201,
  );
  assert.equal((await legacy(["legacy.example:443"])).status, 200);
  assert.equal((await upload(id, token, "two", wasm)).status, 201);
  assert.deepEqual((await caps("two")).net_allow_outbound, []);

  const restricted = [];
  for (const scopes of [["read"], ["read", "deploy"]]) {
    const login = await api("/auth/login", {
      method: "POST",
      body: {
        tenant_slug: "upload",
        email: "test@example.invalid",
        password: "test-password",
        scopes,
      },
    });
    assert.equal(login.status, 201);
    const { token: limited } = await login.json();
    restricted.push(limited);
    assert.deepEqual(await read(`${base}/egress`, limited), {
      allow_outbound: null,
    });
    assert.equal(
      (await change({ allow: ["db.example:5432"] }, limited)).status,
      403,
    );
  }
  assert.equal((await api(`${base}/egress`)).status, 401);
  for (const body of [
    {},
    { allow: ["user:password@host:5432"] },
    { allow: ["*:443"] },
    { allow: ["db:0"] },
    { allow: ["db:443"], deny: ["DB:443"] },
    { allow: ["db:443"], bypass: true },
    { allow: Array(65).fill("db:443") },
  ]) {
    assert.equal((await change(body)).status, 400, JSON.stringify(body));
  }
  assert.deepEqual(await read(`${base}/egress`), { allow_outbound: null });
  // Exercise both current object capabilities and the supported legacy import array.
  const original = JSON.parse(
    (
      await sql(
        `SELECT capabilities FROM component_versions WHERE component_id='${id}' AND version='one'`,
      )
    ).trim(),
  );
  await sql(
    `UPDATE component_versions SET capabilities='["wasi:cli/environment@0.2.0"]' WHERE component_id='${id}' AND version='two'`,
  );
  const first = await change({
    allow: ["DB.Example.:05432", "[2606:4700:4700::1111]:443"],
  });
  assert.equal(first.status, 200, await first.clone().text());
  const approved = ["[2606:4700:4700::1111]:443", "db.example:5432"];
  assert.deepEqual((await first.json()).allow_outbound, approved);
  for (const version of ["one", "two"])
    assert.deepEqual((await caps(version)).net_allow_outbound, approved);
  const rows = JSON.parse(
    (
      await sql(
        `SELECT json_agg(json_build_object('version',version,'caps',capabilities) ORDER BY version) FROM component_versions WHERE component_id='${id}'`,
      )
    ).trim(),
  );
  assert.deepEqual(rows[0].caps.imports, original.imports);
  assert.deepEqual(rows[0].caps.env, ["GREETING"]);
  assert.deepEqual(rows[1].caps.imports, ["wasi:cli/environment@0.2.0"]);
  assert.equal((await legacy(["bypass.example:443"])).status, 409);

  assert.equal(
    (
      await upload(id, restricted[1], "three", wasm, 0, {
        capabilities: { net_allow_outbound: ["unapproved.example:443"] },
      })
    ).status,
    201,
  );
  assert.deepEqual((await caps("three")).net_allow_outbound, approved);
  // Incremental edits from independent clients must both survive serialization.
  const concurrent = await Promise.all([
    change({ allow: ["a.example:443"] }),
    change({ allow: ["b.example:443"] }),
  ]);
  concurrent.forEach((r) => assert.equal(r.status, 200));
  assert.deepEqual(
    (await read(`${base}/egress`)).allow_outbound,
    [...approved, "a.example:443", "b.example:443"].sort(),
  );

  // A policy edit during artifact I/O is picked up at publication, not upload start.
  const held = holdStorage();
  const pending = upload(id, restricted[1], "four", wasm);
  await held;
  try {
    assert.equal((await change({ deny: ["db.example:5432"] })).status, 200);
  } finally {
    releaseStorage();
  }
  assert.equal((await pending).status, 201);
  for (const version of ["one", "two", "three", "four"])
    assert.ok(
      !(await caps(version)).net_allow_outbound.includes("db.example:5432"),
    );
  assert.equal(
    (
      await api(`${base}/rollback`, {
        token: restricted[1],
        method: "POST",
        body: { version: "one" },
      })
    ).status,
    200,
  );
  assert.ok(
    !(await caps("one")).net_allow_outbound.includes("db.example:5432"),
  );
  const remaining = (await read(`${base}/egress`)).allow_outbound;
  assert.equal((await change({ deny: remaining })).status, 200);
  assert.deepEqual(await read(`${base}/egress`), { allow_outbound: [] });
  assert.equal((await upload(id, token, "five", wasm)).status, 201);
  assert.deepEqual((await caps("five")).net_allow_outbound, []);

  assert.equal(
    (
      await api("/admin/tenants", {
        token: "test-only",
        method: "POST",
        body: {
          slug: "egress-other",
          name: "Other",
          admin_email: "other@example.invalid",
          admin_password: "test-password",
        },
      })
    ).status,
    201,
  );
  const other = await (
    await api("/auth/login", {
      method: "POST",
      body: {
        tenant_slug: "egress-other",
        email: "other@example.invalid",
        password: "test-password",
      },
    })
  ).json();
  assert.equal(
    (await api(`${base}/egress`, { token: other.token })).status,
    404,
  );
  assert.equal(
    (await change({ allow: ["foreign.example:443"] }, other.token)).status,
    404,
  );
  await api("/auth/logout", { token: other.token, method: "POST" });
  assert.ok(
    Number(
      (
        await sql(
          `SELECT count(*) FROM audit_logs WHERE action='component_egress_updated' AND target='${id}'`,
        )
      ).trim(),
    ) >= 5,
  );
  assert.equal((await api(base, { token, method: "DELETE" })).status, 204);
  assert.equal((await api(`${base}/egress`, { token })).status, 404);
  assert.equal((await change({ allow: ["deleted.example:443"] })).status, 404);
  const recreated = await create();
  assert.notEqual(recreated, id);
  assert.deepEqual(await read(`/components/${recreated}/egress`), {
    allow_outbound: null,
  });
  await api(`/components/${recreated}`, { token, method: "DELETE" });
  for (const token of restricted)
    await api("/auth/logout", { token, method: "POST" });
  console.log(
    "PASS application egress: admin-only, tenant isolation, legacy migration, inheritance, revocation, rollback, concurrent edits and deployment race",
  );
}
