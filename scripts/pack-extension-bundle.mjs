// Publisher-only packaging. npm owns installation and bundled dependencies;
// the CLI and runtime continue to consume ordinary extension manifests.
import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { packExtensions } from "./pack-extensions.mjs";
import { runCommand } from "./bounded-process.mjs";

const presets = ["postgres-scram", "postgres", "postgres-tcp"];

export async function packExtensionBundle(destination, name) {
  if (!presets.includes(name))
    throw new Error(`Choose one PostgreSQL preset: ${presets.join(", ")}`);
  destination = resolve(destination);
  await mkdir(destination, { recursive: true });
  const temporary = await mkdtemp(join(tmpdir(), "hibana-extension-bundle-"));
  let publishing;
  try {
    const packages = await packExtensions(temporary, [name]);
    const preset = packages.find((pkg) => pkg.name === `@hibana/${name}`);
    const staging = join(temporary, "package");
    await mkdir(staging);
    await runCommand("tar", [
      "-xzf",
      preset.tarball,
      "--strip-components",
      "1",
      "-C",
      staging,
    ]);

    // Explicit local tarballs satisfy all unpublished @hibana dependencies.
    // npm ci in the publisher workspace has already cached the public packages.
    // --no-save keeps the preset's direct dependencies unchanged.
    await runCommand(
      "npm",
      [
        "install",
        "--offline",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
        "--no-save",
        "--package-lock=false",
        ...packages.filter((pkg) => pkg !== preset).map((pkg) => pkg.tarball),
      ],
      { cwd: staging, timeoutMs: 120000 },
    );
    const file = join(staging, "package.json");
    const metadata = JSON.parse(await readFile(file, "utf8"));
    metadata.bundleDependencies = Object.keys(metadata.dependencies);
    await writeFile(file, JSON.stringify(metadata, null, 2) + "\n");

    // Publish by rename on the destination filesystem, even when TMPDIR lives
    // on another volume. Never replace a previous archive with a partial pack.
    publishing = await mkdtemp(join(destination, ".hibana-bundle-"));
    const [packed] = JSON.parse(
      await runCommand(
        "npm",
        [
          "pack",
          "--ignore-scripts",
          "--json",
          "--pack-destination",
          publishing,
        ],
        { cwd: staging, timeoutMs: 120000 },
      ),
    );
    for (const pkg of packages.filter((pkg) => pkg !== preset)) {
      assert.ok(
        packed.files.some(
          (entry) =>
            entry.path === `node_modules/${pkg.name}/hibana.extension.json`,
        ),
        `Missing bundled dependency: ${pkg.name}`,
      );
    }
    // Distinguish the self-contained archive from the normal package of the
    // same version. Source packages, versions and feature boundaries are intact.
    const filename = packed.filename.replace(/\.tgz$/, "-bundle.tgz");
    const tarball = join(destination, filename);
    await rename(join(publishing, packed.filename), tarball);
    return { ...packed, filename, tarball };
  } finally {
    if (publishing) await rm(publishing, { recursive: true, force: true });
    await rm(temporary, { recursive: true, force: true });
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const [name, destination, ...extra] = process.argv.slice(2);
  if (!name || !destination || extra.length)
    throw new Error(
      "Usage: npm run pack:bundle --prefix extensions -- <preset> <output-directory>",
    );
  const { tarball } = await packExtensionBundle(destination, name);
  console.log(tarball);
}
