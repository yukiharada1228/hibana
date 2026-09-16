// Pack actual consumer artifacts, including dependencies, without publisher hooks.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { runCommand } from "./bounded-process.mjs";

export async function packExtensions(destination, names) {
  const root = fileURLToPath(new URL("../extensions", import.meta.url));
  const workspace = JSON.parse(await readFile(join(root, "package.json")));
  const manifests = new Map();
  for (const name of workspace.workspaces)
    manifests.set(
      name,
      JSON.parse(await readFile(join(root, name, "hibana.extension.json"))),
    );
  const selected = new Set();
  function visit(name) {
    assert.ok(manifests.has(name), `Unknown extension: ${name}`);
    if (selected.has(name)) return;
    selected.add(name);
    for (const dependency of manifests.get(name).dependencies || [])
      visit(dependency.replace("@hibana/", ""));
  }
  for (const name of names || workspace.workspaces) visit(name);
  const results = JSON.parse(
    await runCommand(
      "npm",
      [
        "pack",
        "--ignore-scripts",
        "--json",
        "--pack-destination",
        destination,
        ...[...selected].flatMap((name) => ["--workspace", `@hibana/${name}`]),
      ],
      { cwd: fileURLToPath(new URL("../extensions", import.meta.url)) },
    ),
  );
  const wasmPackages = new Set(
    [...manifests]
      .filter(([, m]) => m.components?.length)
      .map(([name]) => name),
  );
  for (const packed of results) {
    const name = packed.name.replace("@hibana/", "");
    const paths = packed.files.map((file) => file.path);
    assert.equal(
      paths.some((path) => path.endsWith(".wasm")),
      wasmPackages.has(name),
      name,
    );
    assert.ok(
      paths.every(
        (path) => !path.startsWith("component/") && path !== "build.mjs",
      ),
      name,
    );
    if (wasmPackages.has(name)) {
      for (const path of [
        "wit/world.wit",
        "dist/NOTICE.txt",
        `dist/${name}.wasm`,
      ])
        assert.ok(paths.includes(path), `${name}: ${path}`);
    }
    if (name === "tcp")
      assert.ok(
        !paths.some(
          (path) => path.includes("rustls") || path.includes("ring-"),
        ),
      );
    if (name === "tls")
      assert.ok(paths.includes("dist/licenses/ring-0.17.14/LICENSE"));
    if (name === "node-stream") {
      assert.ok(paths.includes("dist/readable-stream/LICENSE"));
      assert.ok(paths.includes("dist/NOTICE.txt"));
      assert.ok(
        paths.includes(
          "dist/readable-stream/lib/internal/streams/duplex-type.js",
        ),
      );
      assert.ok(
        !paths.includes("dist/readable-stream/lib/ours/index.js"),
        "Do not ship the upstream Node-only entry",
      );
    }
    if (name === "postgres-core" || name === "postgres-auth-scram")
      assert.ok(paths.includes("dist/licenses/npm-pg-8.23.0/LICENSE"));
    if (name === "postgres-pool")
      assert.ok(paths.includes("dist/licenses/npm-pg-pool-3.14.0/LICENSE"));
    else assert.ok(!paths.some((path) => path.includes("npm-pg-pool-")), name);
    packed.tarball = join(destination, packed.filename);
  }
  return results;
}
