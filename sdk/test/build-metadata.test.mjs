import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  readBuildMetadata,
  withBuildMetadata,
} from "../src/build-metadata.mjs";

const header = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);
const empty = {
  schema_version: 1,
  input: "javascript",
  roots: [],
  extensions: [],
};
const declaration = {
  ...empty,
  roots: ["@example/tcp"],
  extensions: [
    {
      name: "@example/tcp",
      version: "1.0.0",
      dependencies: [],
      permissions: ["outbound-network"],
    },
  ],
};
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");

test("build metadata is part of the artifact digest, reproducible and replaced rather than duplicated", () => {
  const first = withBuildMetadata(header, declaration);
  assert.deepEqual(readBuildMetadata(first), declaration);
  assert.deepEqual(first, withBuildMetadata(first, declaration));
  const updated = withBuildMetadata(first, empty);
  assert.notEqual(hash(first), hash(updated));
  assert.deepEqual(readBuildMetadata(updated), empty);
  assert.equal(readBuildMetadata(header), null);
  assert.deepEqual(
    withBuildMetadata(first, empty, { preserveExisting: true }),
    first,
  );
});

test("nested components cannot supply the outer application's declaration", () => {
  const nested = withBuildMetadata(header, empty);
  assert.ok(nested.length < 128);
  const outer = Buffer.concat([
    header,
    Buffer.from([4, nested.length]),
    nested,
  ]);
  assert.equal(readBuildMetadata(outer), null);
  const result = withBuildMetadata(outer, declaration);
  assert.deepEqual(readBuildMetadata(result), declaration);
  assert.deepEqual(result.subarray(0, outer.length), outer);
});

test("duplicate, oversized and malformed metadata cannot be silently attached", () => {
  const first = withBuildMetadata(header, empty);
  assert.throws(
    () => readBuildMetadata(Buffer.concat([first, first.subarray(8)])),
    /Duplicate/,
  );
  assert.throws(
    () => withBuildMetadata(header, { text: "x".repeat(32768) }),
    /32 KiB/,
  );
  for (const tail of [
    [0, 128],
    [0, 5, 7, 1, 2, 3, 4],
    [0, 255, 255, 255, 255, 31],
    [4, 20, 1],
  ])
    assert.throws(
      () =>
        withBuildMetadata(Buffer.concat([header, Buffer.from(tail)]), empty),
      /section/,
    );
});
