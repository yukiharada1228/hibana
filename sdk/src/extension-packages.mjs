// npm is an implementation detail of extension distribution. App dependencies
// are never installed or rewritten here; only .hibana/ and hibana-lock.json are used.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { run } from "./process.mjs";
import { isDeepStrictEqual } from "node:util";
import { setTimeout as delay } from "node:timers/promises";
import {
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { dirname, join, relative, resolve, isAbsolute } from "node:path";
import {
  isExtensionArchive,
  managesExtensions,
  validateExtensionList,
} from "./extension-manifest.mjs";
import {
  isCompleteInstallation,
  publishInstallation,
} from "./extension-cache.mjs";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const json = (value) => JSON.stringify(value, null, 2) + "\n";
const manifest = (dependencies) => ({
  name: "hibana-application-extensions",
  version: "0.0.0",
  private: true,
  dependencies,
});
const inside = (root, path) => {
  const part = relative(root, path);
  return (
    part !== ".." &&
    !part.startsWith("../") &&
    !part.startsWith("..\\") &&
    !isAbsolute(part)
  );
};

async function readLock(path) {
  try {
    if ((await stat(path)).size > 4 * 1024 * 1024)
      throw new Error("hibana-lock.json exceeds 4 MiB");
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw new Error(`Cannot read hibana-lock.json: ${error.message}`, {
      cause: error,
    });
  }
}

async function commitLock(path, previous, lock) {
  if (isDeepStrictEqual(previous, lock)) return;
  const directory = join(dirname(path), ".hibana");
  await mkdir(directory, { recursive: true });
  // mkdir serializes the comparison and publication across CLI processes.
  // Never steal a timed-out guard: its owner may still be writing the lock.
  const guard = join(directory, "lock-update");
  const deadline = performance.now() + 5000;
  for (;;) {
    try {
      await mkdir(guard);
      break;
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      if (performance.now() >= deadline)
        throw new Error(
          "Timed out waiting to update hibana-lock.json. Retry after other Hibana commands finish. If no Hibana command is running, remove .hibana/lock-update and retry.",
        );
      await delay(25);
    }
  }
  try {
    const current = await readLock(path);
    if (isDeepStrictEqual(current, lock)) return;
    if (!isDeepStrictEqual(current, previous))
      throw new Error(
        "hibana-lock.json changed during extension installation. Retry the build.",
      );
    const candidate = join(guard, "hibana-lock.json");
    await writeFile(candidate, json(lock));
    await rename(candidate, path);
  } finally {
    await rm(guard, { recursive: true, force: true });
  }
}

function validateLock(lock, dependencies) {
  try {
    assert.equal(lock.schemaVersion, 1);
    assert.ok(managesExtensions(lock.sources));
    validateExtensionList(lock.sources);
    assert.ok(managesExtensions(lock.integrity));
    const archives = Object.keys(lock.sources).filter((name) =>
      isExtensionArchive(lock.sources[name]),
    );
    assert.deepEqual(Object.keys(lock.integrity).sort(), archives.sort());
    const lockedDependencies = Object.fromEntries(
      Object.entries(lock.sources).map(([name, source]) => {
        if (!isExtensionArchive(source)) return [name, source];
        assert.match(lock.integrity[name], /^[a-f0-9]{64}$/);
        return [name, `file:sources/${lock.integrity[name]}.tgz`];
      }),
    );
    assert.equal(lock.npm.lockfileVersion, 3);
    assert.equal(lock.npm.name, "hibana-application-extensions");
    assert.deepEqual(lock.npm.packages[""].dependencies, lockedDependencies);
    if (dependencies) assert.deepEqual(lockedDependencies, dependencies);
    for (const [path, pkg] of Object.entries(lock.npm.packages)) {
      assert.ok(path === "" || /^node_modules\//.test(path));
      assert.ok(!path.includes("\\") && !path.split("/").includes(".."));
      assert.ok(!pkg.link);
      if (!path || pkg.inBundle) continue;
      assert.match(pkg.integrity, /^sha512-[A-Za-z0-9+/]+={0,2}$/);
      if (pkg.resolved?.startsWith("file:")) {
        assert.match(pkg.resolved, /^file:sources\/[a-f0-9]{64}\.tgz$/);
        assert.ok(Object.values(lockedDependencies).includes(pkg.resolved));
      } else {
        const url = new URL(pkg.resolved);
        assert.equal(url.protocol, "https:");
        assert.ok(!url.username && !url.password);
      }
    }
  } catch (error) {
    throw new Error(
      "Invalid hibana-lock.json: extension dependencies must have pinned sources and integrity",
      { cause: error },
    );
  }
}

async function npm(command, directory, signal) {
  try {
    await run(
      "npm",
      [
        command,
        "--ignore-scripts",
        "--no-bin-links",
        "--no-audit",
        "--no-fund",
        "--omit=dev",
        "--workspaces=false",
      ],
      {
        cwd: directory,
        capture: true,
        signal,
        timeout: 120000,
        maxBuffer: 2 * 1024 * 1024,
      },
    );
  } catch (error) {
    throw new Error(
      `Could not install Hibana extensions: ${error.stderr?.trim() || error.message}`,
      { cause: error },
    );
  }
}

export async function installExtensionPackages(
  config,
  { frozenLockfile = false, signal } = {},
) {
  const sources = Object.fromEntries(
    Object.entries(config.extensions || {}).sort(([a], [b]) =>
      a.localeCompare(b),
    ),
  );
  const project = await realpath(config.root);
  const dependencies = {},
    archives = new Map(),
    integrity = {};
  for (const [name, source] of Object.entries(sources)) {
    if (isExtensionArchive(source)) {
      const path = await realpath(resolve(project, source));
      if (!inside(project, path))
        throw new Error(
          "Extension archives must stay inside the project, including symlinks",
        );
      if (
        !(await stat(path)).isFile() ||
        (await stat(path)).size > 128 * 1024 * 1024
      )
        throw new Error("Extension archives must be files of at most 128 MiB");
      const bytes = await readFile(path);
      const digest = hash(bytes);
      const file = `sources/${digest}.tgz`;
      archives.set(file, bytes);
      integrity[name] = digest;
      dependencies[name] = `file:${file}`;
    } else dependencies[name] = source;
  }

  const lockPath = join(project, "hibana-lock.json");
  const previous = await readLock(lockPath);
  const empty = !Object.keys(sources).length;
  // Plain applications need no extension lock. Removing previously locked
  // extensions, however, must go through the same frozen check and lock update.
  if (empty && !previous) return { root: project, managed: true };
  const matches = previous && isDeepStrictEqual(previous.sources, sources);
  if (frozenLockfile && !matches)
    throw new Error(
      "hibana-lock.json is missing or differs from hibana.json. Run hibana build locally and commit hibana-lock.json.",
    );
  for (const [name, digest] of Object.entries(integrity)) {
    if (
      previous?.sources?.[name] === sources[name] &&
      previous.integrity?.[name] !== digest
    )
      throw new Error(
        `Extension archive integrity differs from hibana-lock.json for ${name}. Restore the archive, or use a new source path for an intentional update.`,
      );
  }
  if (previous) validateLock(previous, matches ? dependencies : undefined);
  if (matches) {
    if (!isDeepStrictEqual(previous.integrity, integrity))
      throw new Error(
        "Invalid hibana-lock.json: archive integrity does not match the extension sources",
      );
  }
  if (empty) {
    if (matches) return { root: project, managed: true };
    const { name, version } = manifest({});
    const lock = {
      schemaVersion: 1,
      sources: {},
      integrity: {},
      npm: {
        name,
        version,
        lockfileVersion: 3,
        requires: true,
        packages: { "": { name, version, dependencies: {} } },
      },
    };
    return {
      root: project,
      managed: true,
      commitLock: () => commitLock(lockPath, previous, lock),
    };
  }
  const directory = join(project, ".hibana/extensions");
  await mkdir(directory, { recursive: true });
  if (matches) {
    const cached = join(directory, hash(json(previous)));
    if (await isCompleteInstallation(cached, hash(json(previous))))
      return { root: cached, managed: true };
  }

  const staging = await mkdtemp(join(directory, "staging-"));
  try {
    await mkdir(join(staging, "sources"));
    for (const [file, bytes] of archives)
      await writeFile(join(staging, file), bytes);
    await writeFile(
      join(staging, "package.json"),
      json(manifest(dependencies)),
    );
    // npm install applies source changes to the existing resolution. Starting
    // without it would also upgrade unrelated dependencies with version ranges.
    if (previous)
      await writeFile(join(staging, "package-lock.json"), json(previous.npm));
    console.error(
      matches
        ? "Restoring locked Hibana extensions..."
        : "Preparing Hibana extensions from hibana.json...",
    );
    await npm(matches ? "ci" : "install", staging, signal);
    for (const name of Object.keys(sources)) {
      const pkg = JSON.parse(
        await readFile(
          join(staging, "node_modules", name, "package.json"),
          "utf8",
        ),
      );
      if (pkg.name !== name)
        throw new Error(`Extension package name does not match ${name}`);
    }
    const lock = matches
      ? previous
      : {
          schemaVersion: 1,
          sources,
          integrity,
          npm: JSON.parse(
            await readFile(join(staging, "package-lock.json"), "utf8"),
          ),
        };
    validateLock(lock, dependencies);
    const key = hash(json(lock)),
      cached = join(directory, key);
    await publishInstallation(staging, cached, key);
    return {
      root: cached,
      managed: true,
      commitLock: () => commitLock(lockPath, previous, lock),
    };
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}
