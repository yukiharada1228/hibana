import test from "node:test";
import assert from "node:assert/strict";
import { cp, mkdtemp, readFile, writeFile, rm, access } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { build } from "../src/build.mjs";
import { readBuildMetadata, withBuildMetadata } from "../src/build-metadata.mjs";
import { init } from "../src/init.mjs";
import { loadConfig } from "../src/config.mjs";
import { shouldRebuild } from "../src/dev.mjs";
import { deploy } from "../src/api.mjs";

const component = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);
async function fixture(fn) {
  const root = await mkdtemp(join(tmpdir(), "hibana-build-test-"));
  try { await fn(root); } finally { await rm(root, { recursive: true, force: true }); }
}

test("native build commands run in order with literal argv; failed builds preserve the last artifact", () => fixture(async root => {
  const literal = 'spaces; $(touch injected) `touch injected` $HOME';
  await writeFile(join(root, "compile.mjs"), `import { writeFileSync } from 'node:fs'; writeFileSync('argv.txt', process.argv[2]); writeFileSync('source.wasm', Buffer.from(${JSON.stringify([...component])}));`);
  const config = { root, component: "source.wasm", build: { commands: [[process.execPath, "compile.mjs", literal], [process.execPath, "-e", "require('node:fs').accessSync('source.wasm')"]] } };
  const artifact = await build(config);
  const built = await readFile(artifact);
  assert.equal(await readFile(join(root, "argv.txt"), "utf8"), literal);
  await assert.rejects(access(join(root, "injected")));
  config.build.commands = [[process.execPath, "-e", "require('node:fs').writeFileSync('source.wasm', 'broken'); process.exit(7)"]];
  await assert.rejects(build(config), /failed \(7\)/);
  assert.deepEqual(await readFile(artifact), built);
}));

test("prebuilt Components bypass compilation; core Wasm is rejected before deployment", () => fixture(async root => {
  const input = join(root, "source.wasm");
  const config = { root, component: input };
  await writeFile(input, component);
  const artifact = await build(config);
  const built = await readFile(artifact);
  assert.deepEqual(built.subarray(0, 8), component);
  assert.deepEqual(readBuildMetadata(built), {schema_version:1,input:"component",roots:[],extensions:[]});
  await writeFile(input, Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]));
  await assert.rejects(build(config), /WebAssembly Component/);
  assert.deepEqual(await readFile(artifact), built);
}));

test("deploy keeps its build snapshot while another build replaces the latest artifact", () => fixture(async root => {
  const tagged = tag => Buffer.concat([component, Buffer.from([0, 3, 1, 120, tag])]);
  await writeFile(join(root, "first.wasm"), tagged(65));
  await writeFile(join(root, "second.wasm"), tagged(66));
  const config = { root, name: "app", component: "first.wasm", vars: {}, secrets: [], resources: {} };
  const first = await build(config);
  const expected = await readFile(first);
  const held = Promise.withResolvers(), started = Promise.withResolvers();
  let uploaded;
  const api = { request: async (path, options) => {
    if (path === "/components") {
      started.resolve();
      await held.promise;
      return [{ name: "app", component_id: "fixture" }];
    }
    uploaded = Buffer.from(await options.body.get("wasm").arrayBuffer());
    return {};
  } };
  const pending = deploy(api, config, first, "first");
  try {
    await started.promise;
    const second = await build({ ...config, component: "second.wasm" });
    assert.notEqual(first, second);
    assert.deepEqual(await readFile(first), expected);
    assert.notDeepEqual(await readFile(second), expected);
    assert.deepEqual(await readFile(join(root, ".hibana/build/app.wasm")), await readFile(second));
    // Rebuilding the same bytes reuses a snapshot without mutating it.
    assert.equal(await build(config), first);
    await writeFile(join(root, ".hibana/build/app.wasm"), "external edit");
    assert.deepEqual(await readFile(first), expected);
  } finally {
    held.resolve();
    await pending;
  }
  assert.deepEqual(uploaded, expected);
}));

test("native CLI builds without any installed JavaScript compiler packages", () => fixture(async root => {
  await cp(new URL("../src/", import.meta.url), join(root, "src"), { recursive: true });
  await writeFile(join(root, "app.wasm"), component);
  await writeFile(join(root, "hibana.json"), JSON.stringify({ name: "native", component: "app.wasm" }));
  execFileSync(process.execPath, [join(root, "src/cli.mjs"), "build"], { cwd: root, stdio: "pipe" });
  assert.deepEqual(readBuildMetadata(await readFile(join(root, ".hibana/build/app.wasm"))), {schema_version:1,input:"component",roots:[],extensions:[]});
}));

test("deploying an existing artifact without extra extensions preserves its original build declaration", () => fixture(async root => {
  const original = withBuildMetadata(component, { schema_version: 1, input: "javascript", roots: ["./compat"], extensions: [
    { name: "./compat", version: null, dependencies: [], permissions: [] },
  ] });
  await writeFile(join(root, "source.wasm"), original);
  const output = await build({ root, component: "source.wasm" });
  assert.deepEqual(await readFile(output), original);
}));

test("native config validates commands and watch paths", () => fixture(async root => {
  const path = join(root, "hibana.json");
  for (const extra of [
    { build: { commands: "go build" } }, { build: { commands: [] } },
    { build: { commands: [[""]] } }, { build: { commands: [["go", null]] } },
    { build: { commands: [["go"]], shell: true } },
    { build: { commands: [["go"]], watch: ["../outside"] } },
    { build: { commands: [["go"]], watch: ["src/**"] } },
    { main: "index.ts" }, { component: "" },
  ]) {
    await writeFile(path, JSON.stringify({ name: "native", component: "app.wasm", ...extra }));
    await assert.rejects(loadConfig(path));
  }
  await writeFile(path, JSON.stringify({ name: "native", component: "app.wasm", build: { commands: [["go", "build"]], watch: ["src", "go.mod"] } }));
  const config = await loadConfig(path);
  for (const file of ["src/handler.go", "go.mod", "hibana.json", ".dev.vars"]) assert.equal(shouldRebuild(config, file), true, file);
  for (const file of ["app.wasm", "target/tmp", "src/node_modules/file", "wit_exports.go", "src-other/file"]) assert.equal(shouldRebuild(config, file), false, file);
}));

for (const template of ["javascript", "rust", "go"]) {
  test(`init ${template} produces a portable project without a Hono dependency`, () => fixture(async root => {
    await init(root, { template, install: false });
    const config = await loadConfig(join(root, "hibana.json"));
    if (template === "javascript") {
      const pkg = JSON.parse(await readFile(join(root, "package.json"), "utf8"));
      assert.equal(pkg.dependencies, undefined);
      const metadata = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
      assert.deepEqual(pkg.devDependencies, { [metadata.name]: metadata.version });
      assert.deepEqual(pkg.scripts, { dev: "hibana dev", build: "hibana build", deploy: "hibana deploy" });
      assert.match(await readFile(join(root, config.main), "utf8"), /fetch\(/);
    } else {
      assert.ok(config.build.commands.length);
      assert.match(await readFile(join(root, "wit/world.wit"), "utf8"), /incoming-handler@0.2.3/);
      await access(join(root, "wit/deps/http/types.wit"));
      await assert.rejects(access(join(root, "package.json")));
    }
  }));
}
