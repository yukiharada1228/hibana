import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { installRuntime, runtimePath, runtimeTarget, releaseChecksum } from "../src/runtime.mjs";

async function fixture(t) {
  const home = await mkdtemp(join(tmpdir(), "hibana-runtime-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const bytes = Buffer.from("test-runtime-bytes");
  const checksum = createHash("sha256").update(bytes).digest("hex");
  const target = "linux-x64", version = "0.1.0";
  const filename = `hibana-worker-${version}-${target}`;
  return { home, bytes, checksum, target, version, filename };
}

test("runtime release selection verifies checksums and follows HTTPS asset redirects", async t => {
  const f = await fixture(t), seen = [];
  const fetcher = async (url, options) => {
    seen.push(String(url)); assert.equal(options.redirect, "manual");
    if (String(url).endsWith("SHA256SUMS")) return new Response(`${f.checksum}  ${f.filename}\n`);
    if (url.hostname === "github.com") return new Response(null, { status: 302, headers: { location: "https://release-assets.githubusercontent.com/runtime?download=1" } });
    return new Response(f.bytes);
  };
  const path = await installRuntime({ version: f.version }, { ...f, fetcher });
  assert.deepEqual(await readFile(path), f.bytes);
  assert.equal((await stat(path)).mode & 0o777, 0o700);
  assert.equal(seen.length, 3);
  assert.ok(seen[1].endsWith(`/v${f.version}/${f.filename}`));
});

test("corrupt, incomplete and failed runtime downloads preserve the existing executable", async t => {
  const f = await fixture(t), source = join(f.home, "source");
  await writeFile(source, f.bytes);
  const path = await installRuntime({ version: f.version, from: source, sha256: f.checksum }, f);
  for (const mode of ["wrong-hash", "missing", "http", "oversized", "truncated", "unpublished"]) {
    const fetcher = async url => {
      if (String(url).endsWith("SHA256SUMS")) return new Response(mode === "missing" ? "" : `${f.checksum}  ${f.filename}\n`);
      if (mode === "http") return new Response(null, { status: 302, headers: { location: "http://insecure.example/runtime" } });
      if (mode === "oversized") return new Response("x", { headers: { "content-length": String(200 * 1024 * 1024) } });
      if (mode === "truncated") return new Response(new ReadableStream({ start(c) { c.error(new Error("connection lost")); } }));
      if (mode === "unpublished") return new Response(null, { status: 404 });
      return new Response("corrupt");
    };
    await assert.rejects(installRuntime({ version: f.version }, { ...f, fetcher }), undefined, mode);
    assert.deepEqual(await readFile(path), f.bytes, mode);
  }
});

test("offline runtime installation requires an exact checksum and a supported version/target", async t => {
  const f = await fixture(t), from = join(f.home, "source");
  await writeFile(from, f.bytes);
  await assert.rejects(installRuntime({ version: f.version, from }, f), /both/);
  await assert.rejects(installRuntime({ version: f.version, from, sha256: "0".repeat(64) }, f), /checksum mismatch/);
  await assert.rejects(stat(runtimePath(f.version, f)), { code: "ENOENT" });
  assert.throws(() => runtimePath("../outside", f), /release version/);
  assert.throws(() => runtimeTarget("win32", "x64"), /remote deployment/);
  assert.throws(() => runtimeTarget("linux", "arm"), /remote deployment/);
  assert.throws(() => releaseChecksum(`${f.checksum}  ${f.filename}\n${f.checksum}  ${f.filename}`, f.filename), /duplicated/);
});
