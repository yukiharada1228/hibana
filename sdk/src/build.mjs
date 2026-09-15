import { copyFile, mkdir, mkdtemp, open, rename, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { run } from "./process.mjs";
import { resolveExtensions, prepareExtensionWit } from "./extensions.mjs";

const COMPONENT_HEADER = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);

// A quick format check, not ABI/security validation; the server validates the full Component.
async function checkComponent(path) {
  const file = await open(path, "r");
  try {
    const header = Buffer.alloc(8);
    const { bytesRead } = await file.read(header, 0, 8, 0);
    if (bytesRead !== 8 || !header.equals(COMPONENT_HEADER)) {
      throw new Error(
        "Build output must be a WebAssembly Component; core Wasm / wasip1 modules need componentization and a supported WIT world",
      );
    }
  } finally {
    await file.close();
  }
}

export async function build(config, options = {}) {
  const extensions = await resolveExtensions(config, options);
  if (extensions.permissions.includes("outbound-network"))
    console.error(
      "Extension requires outbound-network. Deployment does not grant network access; an administrator must approve destinations for the deployed version.",
    );
  const directory = join(config.root, ".hibana/build");
  await mkdir(directory, { recursive: true });
  const work = await mkdtemp(join(directory, "staging-"));
  const candidate = join(work, "app.wasm");
  const artifact = join(directory, "app.wasm");
  try {
    const wit = await prepareExtensionWit(extensions, work);
    if (config.main) {
      // Native languages and prebuilt Components do not load the JS toolchain.
      let compileJavaScript;
      try {
        ({ compileJavaScript } = await import("./javascript.mjs"));
      } catch (error) {
        if (error.code === "ERR_MODULE_NOT_FOUND")
          throw new Error(
            "JavaScript builds require the optional compiler dependencies; run npm install --include=optional in the Hibana CLI directory",
            { cause: error },
          );
        throw error;
      }
      await compileJavaScript(config, candidate, { ...extensions, wit });
    } else {
      for (const [command, ...args] of config.build?.commands || []) {
        await run(command, args, { cwd: config.root });
      }
      await copyFile(resolve(config.root, config.component), candidate);
    }
    await checkComponent(candidate);
    let output = candidate;
    if (extensions.components.length) {
      const paths = extensions.components;
      for (const path of paths) await checkComponent(path);
      const composed = join(work, "composed.wasm");
      try {
        await run(
          "wac",
          [
            "plug",
            candidate,
            ...paths.flatMap((path) => ["--plug", path]),
            "-o",
            composed,
          ],
          { cwd: config.root },
        );
      } catch (error) {
        throw new Error(
          `Could not compose application extensions. Install the wac CLI and check that extension exports match application imports. ${error.message}`,
          { cause: error },
        );
      }
      await checkComponent(composed);
      output = composed;
    }
    await rename(output, artifact);
    return artifact;
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}
