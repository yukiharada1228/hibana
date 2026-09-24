// Reproduce a consumer lockfile that retained Jco 1.34.0 after upgrading rc.8.
// Never execute the old compiler; only build after dedupe, audit and npm ci.
import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

const baseline = {
  version: "0.2.0-rc.8",
  tarball:
    "https://registry.npmjs.org/@yukiharada1228/hibana/-/hibana-0.2.0-rc.8.tgz",
  integrity:
    "sha512-b763SijF0zKvoB1F0vfT7Fe88SSO2q3Q2U3t9HCESoQNDafq5U/wTeA+gkeLK6X6YE/xRD0O5ofNpDQHdakYYA==",
};
const jco = "@bytecodealliance/jco";
const componentize = "@bytecodealliance/componentize-js";
const json = async (path) => JSON.parse(await readFile(path, "utf8"));

export async function verifyUpgrade({ run, directory, metadata, source }) {
  await mkdir(join(directory, "src"), { recursive: true });
  const manifest = await json(join(source, "package.json"));
  manifest.devDependencies[metadata.name] = baseline.tarball;
  // Seed the hoisted versions selected by the old resolver, then remove these
  // direct dependencies. Both must survive as transitive dependencies of rc.8.
  manifest.devDependencies[jco] = "1.34.0";
  manifest.devDependencies[componentize] = "0.19.3";
  manifest.scripts.test = "node --version";
  const packageFile = join(directory, "package.json");
  const lockFile = join(directory, "package-lock.json");
  const saveManifest = () =>
    writeFile(packageFile, JSON.stringify(manifest, null, 2) + "\n");
  await saveManifest();
  const preserved = new Map();
  for (const file of ["src/index.ts", "hibana.json"]) {
    const contents = await readFile(join(source, file));
    preserved.set(file, contents);
    await writeFile(join(directory, file), contents);
  }
  const flags = ["--no-audit", "--no-fund"];
  await run("npm", ["install", "--ignore-scripts", ...flags], directory);
  delete manifest.devDependencies[jco];
  delete manifest.devDependencies[componentize];
  await saveManifest();
  await run("npm", ["install", "--ignore-scripts", ...flags], directory);
  const oldLock = await json(lockFile);
  assert.equal(
    oldLock.packages[`node_modules/${metadata.name}`].version,
    baseline.version,
  );
  assert.equal(
    oldLock.packages[`node_modules/${metadata.name}`].integrity,
    baseline.integrity,
  );
  assert.equal(oldLock.packages[`node_modules/${jco}`].version, "1.34.0");
  assert.equal(
    oldLock.packages[`node_modules/${componentize}`].version,
    "0.19.3",
  );
  assert.equal(oldLock.packages[""].devDependencies[jco], undefined);
  assert.equal(oldLock.packages[""].devDependencies[componentize], undefined);
  console.log(
    "Upgrade fixture: rc.8 with an existing lockfile and transitive Jco 1.34.0 installed.",
  );

  // Follow the documented upgrade with both node_modules and the lock intact.
  await run(
    "npm",
    [
      "install",
      "--save-dev",
      "--save-exact",
      `${metadata.name}@${metadata.version}`,
      "--ignore-scripts",
      ...flags,
    ],
    directory,
  );
  const retained = await json(lockFile);
  assert.equal(
    retained.packages[`node_modules/${jco}`].version,
    "1.34.0",
    "fixture must reproduce the retained old compiler before dedupe",
  );
  await run("npm", ["dedupe", "--prefer-dedupe", ...flags], directory);
  const updated = await json(packageFile);
  assert.deepEqual(updated, {
    ...manifest,
    devDependencies: {
      ...manifest.devDependencies,
      [metadata.name]: metadata.version,
    },
  });
  const lock = await json(lockFile);
  assert.equal(
    lock.packages[`node_modules/${metadata.name}`].version,
    metadata.version,
  );
  for (const [name, version] of Object.entries(metadata.optionalDependencies)) {
    const installations = Object.entries(lock.packages).filter(([path]) =>
      path.endsWith(`node_modules/${name}`),
    );
    assert.ok(installations.length > 0, `${name} is installed`);
    for (const [path, dependency] of installations)
      assert.equal(
        dependency.version,
        version,
        `${path} is the audited compiler version`,
      );
  }
  await run("npm", ["audit", "--audit-level=low"], directory);
  const savedLock = await readFile(lockFile);
  // A second developer / CI must reproduce the repaired tree from this lock.
  await run("npm", ["ci", ...flags], directory);
  assert.deepEqual(await readFile(lockFile), savedLock);
  for (const [file, contents] of preserved)
    assert.deepEqual(
      await readFile(join(directory, file)),
      contents,
      `${file} is preserved`,
    );
  const cli = join(directory, "node_modules", metadata.name, "src/cli.mjs");
  assert.equal(
    (await run(process.execPath, [cli, "--version"], directory)).trim(),
    `hibana ${metadata.version}`,
  );
  await run("npm", ["run", "build"], directory, { npm_config_offline: "true" });
  const wasm = await readFile(join(directory, ".hibana/build/app.wasm"));
  assert.deepEqual(
    wasm.subarray(0, 8),
    Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]),
  );
  console.log(
    `rc.8 → ${metadata.version}: dedupe, dependency audit, npm ci and Hono build passed; source, configuration and custom scripts preserved.`,
  );
}
