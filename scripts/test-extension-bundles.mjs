// The publisher starts with an empty npm cache and may resolve public packages.
// Consumer installs have empty caches and no registry access. Each preset must
// work from hibana.json alone, without application npm dependencies.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { packExtensionBundle } from "./pack-extension-bundle.mjs";
import { runCommand } from "./bounded-process.mjs";
import { resolveExtensions } from "../sdk/src/extensions.mjs";
import { extensionImports } from "../sdk/src/extension-imports.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));
const { build } = createRequire(join(root, "sdk/package.json"))("esbuild");
const temporary = await mkdtemp(
  join(tmpdir(), "hibana-extension-bundles-test-"),
);
const readJson = async (path) => JSON.parse(await readFile(path, "utf8"));
const publisherCache = process.env.npm_config_cache;
process.env.npm_config_cache = join(temporary, "publisher-cache");

try {
  await assert.rejects(
    packExtensionBundle(temporary, "node-tls"),
    /Choose one PostgreSQL preset/,
  );
  for (const [name, expectedPackages, expectedWasm] of [
    ["postgres-scram", 20, 8],
    ["postgres", 27, 14],
    ["postgres-tcp", 19, 8],
  ]) {
    const reference = `@hibana/${name}`;
    const packed = await packExtensionBundle(temporary, name);
    const app = join(temporary, name);
    await mkdir(join(app, "vendor"), { recursive: true });
    await cp(packed.tarball, join(app, "vendor", packed.filename));
    const dependencies = { [reference]: `file:vendor/${packed.filename}` };
    await writeFile(
      join(app, "package.json"),
      JSON.stringify({ private: true, dependencies }),
    );
    const source = 'export {Client, Pool} from "pg";';
    await writeFile(join(app, "index.mjs"), source);
    const npmOptions = [
      "--offline",
      "--ignore-scripts",
      "--no-audit",
      "--no-fund",
    ];
    await runCommand(
      "npm",
      ["install", ...npmOptions, "--cache", join(app, "empty-cache")],
      { cwd: app },
    );
    // A new empty cache proves the lockfile and archive are sufficient for CI.
    await runCommand(
      "npm",
      ["ci", ...npmOptions, "--cache", join(app, "another-empty-cache")],
      { cwd: app },
    );
    await runCommand("npm", ["ls", "--all", "--omit=dev"], { cwd: app });
    assert.deepEqual(
      (await readJson(join(app, "package.json"))).dependencies,
      dependencies,
    );
    const lock = await readJson(join(app, "package-lock.json"));
    assert.deepEqual(lock.packages[""].dependencies, dependencies);
    for (const [path, pkg] of Object.entries(lock.packages)) {
      if (!path || path === `node_modules/${reference}`) continue;
      assert.equal(
        pkg.inBundle,
        true,
        `Registry dependency escaped the bundle: ${path}`,
      );
    }

    const legacy = await resolveExtensions({
      root: app,
      main: "index.mjs",
      extensions: [reference],
    });
    const managed = join(app, "managed");
    await mkdir(join(managed, "vendor"), { recursive: true });
    await cp(packed.tarball, join(managed, "vendor", packed.filename));
    const config = {
      root: managed,
      main: "index.mjs",
      extensions: { [reference]: `./vendor/${packed.filename}` },
    };
    const savedCache = process.env.npm_config_cache;
    const savedOffline = process.env.npm_config_offline;
    let plan;
    try {
      process.env.npm_config_offline = "true";
      process.env.npm_config_cache = join(managed, "empty-cache");
      plan = await resolveExtensions(config);
      const metadata = plan.metadata;
      await rm(join(managed, ".hibana"), { recursive: true });
      process.env.npm_config_cache = join(managed, "another-empty-cache");
      plan = await resolveExtensions(config, { frozenLockfile: true });
      assert.deepEqual(plan.metadata, metadata);
    } finally {
      if (savedCache === undefined) delete process.env.npm_config_cache;
      else process.env.npm_config_cache = savedCache;
      if (savedOffline === undefined) delete process.env.npm_config_offline;
      else process.env.npm_config_offline = savedOffline;
    }
    for (const file of ["package.json", "package-lock.json", "node_modules"])
      await assert.rejects(readFile(join(managed, file)), { code: "ENOENT" });
    assert.deepEqual(plan.metadata, legacy.metadata);
    const expected = await resolveExtensions({
      ...config,
      root: join(root, "extensions"),
      extensions: [`./${name}`],
    });
    expected.metadata.roots = [reference];
    expected.metadata.extensions.find(
      (entry) => entry.name === `./${name}`,
    ).name = reference;
    assert.deepEqual(plan.metadata, expected.metadata);
    assert.equal(plan.metadata.extensions.length, expectedPackages);
    assert.equal(plan.components.length, expectedWasm);
    assert.deepEqual(
      plan.components.map((path) => basename(path)),
      expected.components.map((path) => basename(path)),
    );
    assert.deepEqual(plan.imports, expected.imports);
    assert.deepEqual(plan.permissions, expected.permissions);
    for (let i = 0; i < plan.components.length; i++)
      assert.deepEqual(
        await readFile(plan.components[i]),
        await readFile(expected.components[i]),
      );
    // Resolve the aliases and nested JS dependencies as the app compiler does.
    const output = await build({
      stdin: {
        contents: [
          ...plan.preload.map((path) => `import ${JSON.stringify(path)};`),
          source,
        ].join("\n"),
        resolveDir: managed,
      },
      absWorkingDir: managed,
      bundle: true,
      platform: "browser",
      format: "esm",
      mainFields: ["module", "main"],
      conditions: ["import", "default"],
      alias: plan.aliases,
      plugins: [extensionImports(plan.packages)],
      external: plan.imports,
      metafile: true,
      write: false,
    });
    assert.ok(output.outputFiles[0].contents.length > 0);
    const imports = Object.values(output.metafile.outputs)[0].imports;
    assert.ok(imports.every((entry) => plan.imports.includes(entry.path)));
    console.log(
      `PASS ${reference}: hibana.json only, offline frozen restore, ${expectedPackages} extensions, ${expectedWasm} unchanged Wasm components`,
    );
  }
} finally {
  if (publisherCache === undefined) delete process.env.npm_config_cache;
  else process.env.npm_config_cache = publisherCache;
  await rm(temporary, { recursive: true, force: true });
}
