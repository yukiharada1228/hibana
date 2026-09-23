import { issueFixtureToken } from "./test-api-credentials.mjs";
// Runs only inside the disposable test-http.sh platform.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { withBuildMetadata } from "../sdk/src/build-metadata.mjs";

export async function testBuildMetadata({
  api,
  sql,
  token,
  wasm,
  unrecordedWasm,
  upload,
}) {
  const created = await api("/components", {
    token,
    method: "POST",
    body: { name: "extension-metadata" },
  });
  assert.equal(created.status, 201);
  const { component_id: id } = await created.json();
  const base = `/components/${id}`;
  const metadata = {
    schema_version: 1,
    input: "javascript",
    roots: ["./database"],
    extensions: [
      {
        name: "./database",
        version: null,
        dependencies: ["@example/tcp"],
        permissions: [],
      },
      {
        name: "@example/tcp",
        version: "1.2.3",
        dependencies: [],
        permissions: ["outbound-network"],
      },
    ],
  };
  const bytes = withBuildMetadata(wasm, metadata);
  assert.equal((await upload(id, token, "with-extensions", bytes)).status, 201);
  // Login with Read only. Build declarations never grant network access.
  const reader = await (
    await issueFixtureToken(sql, {
        tenant_slug: "upload",
        email: "test@example.invalid",
        scopes: ["read"],
      })
  ).json();
  const details = async (version) => {
    const response = await api(`${base}/versions/${version}`, {
      token: reader.token,
    });
    assert.equal(response.status, 200);
    return response.json();
  };
  try {
    const stored = await details("with-extensions");
    assert.deepEqual(stored.build_metadata, metadata);
    assert.equal(
      stored.wasm_sha256,
      createHash("sha256").update(bytes).digest("hex"),
    );
    assert.deepEqual(Object.keys(stored).sort(), [
      "build_metadata",
      "version_id",
      "wasm_sha256",
    ]);
    assert.equal((await api(`${base}/versions/with-extensions`)).status, 401);
    assert.equal(
      (
        await api(`${base}/versions/with-extensions`, {
          token: reader.token,
          method: "DELETE",
        })
      ).status,
      403,
    );
    assert.equal(
      (
        await api("/admin/tenants", {
          token: "test-only",
          method: "POST",
          body: {
            slug: "metadata-other",
            name: "Other",
            admin_email: "other@example.invalid",
            admin_oidc_subject: 'fixture-admin',
          },
        })
      ).status,
      201,
    );
    const other = await (
      await issueFixtureToken(sql, {
          tenant_slug: "metadata-other",
          email: "other@example.invalid",
          })
    ).json();
    try {
      assert.equal(
        (await api(`${base}/versions/with-extensions`, { token: other.token }))
          .status,
        404,
      );
    } finally {
      await api("/auth/logout", { token: other.token, method: "POST" });
    }
    const before = await (await api(`${base}/versions`, { token })).json();
    assert.ok(
      before.every((v) => !Object.hasOwn(v, "build_metadata")),
      "list stays lightweight",
    );
    for (const invalid of [
      { ...metadata, roots: ["missing"] },
      {
        ...metadata,
        extensions: metadata.extensions.map((e) => ({
          ...e,
          permissions: ["filesystem"],
        })),
      },
      { ...metadata, net_allow_outbound: ["*:5432"] },
    ]) {
      const rejected = await upload(
        id,
        token,
        "invalid",
        withBuildMetadata(wasm, invalid),
      );
      assert.equal(rejected.status, 400);
      assert.equal(rejected.data.error.code, "invalid_request");
      assert.match(
        rejected.data.error.message,
        /invalid Hibana build metadata/,
      );
    }
    assert.equal(
      (await (await api(`${base}/versions`, { token })).json()).length,
      before.length,
    );
    const empty = {
      schema_version: 1,
      input: "javascript",
      roots: [],
      extensions: [],
    };
    assert.equal(
      (await upload(id, token, "no-extensions", withBuildMetadata(wasm, empty)))
        .status,
      201,
    );
    assert.deepEqual((await details("no-extensions")).build_metadata, empty);
    assert.equal(
      (
        await api(`${base}/rollback`, {
          token,
          method: "POST",
          body: { version: "with-extensions" },
        })
      ).status,
      200,
    );
    assert.deepEqual(
      (await details("with-extensions")).build_metadata,
      metadata,
    );
    assert.equal(
      (
        await upload(id, token, "legacy", unrecordedWasm, 0, {
          activate: false,
        })
      ).status,
      201,
    );
    assert.equal((await details("legacy")).build_metadata, null);
    assert.equal(
      (await api(`${base}/versions/legacy`, { token, method: "DELETE" }))
        .status,
      204,
    );
    assert.equal(
      (await api(`${base}/versions/legacy`, { token: reader.token })).status,
      404,
    );
    assert.equal(
      (await api(`${base}/versions/absent`, { token: reader.token })).status,
      404,
    );
    assert.equal((await api(base, { token, method: "DELETE" })).status, 204);
    assert.equal(
      (await api(`${base}/versions/with-extensions`, { token: reader.token }))
        .status,
      404,
    );
  } finally {
    await api("/auth/logout", { token: reader.token, method: "POST" });
  }
  console.log(
    "PASS build metadata: artifact digest binding, Read access, validation, empty versions, rollback, deleted parent and no permission grants",
  );
}
