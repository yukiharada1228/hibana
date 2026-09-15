// Resolve installed packages and local directories through one declarative format.
// This code never imports extension entry points or runs installation/build hooks.
import {
  cp,
  mkdir,
  readFile,
  readdir,
  realpath,
  stat,
  writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import {
  HTTP_CONTRACT,
  isLocalExtension,
  validateExtensionList,
  validateManifest,
} from "./extension-manifest.mjs";

const MANIFEST = "hibana.extension.json";

function isInside(root, path) {
  const part = relative(root, path);
  return (
    part !== ".." &&
    !part.startsWith("../") &&
    !part.startsWith("..\\") &&
    !isAbsolute(part)
  );
}

async function packagePath(root, input, directory = false) {
  if (
    typeof input !== "string" ||
    !input.startsWith("./") ||
    input.includes("\0") ||
    input.includes("\\") ||
    input.split("/").includes("..")
  ) {
    throw new Error(
      "Extension paths must start with ./ and stay inside the package",
    );
  }
  const path = await realpath(resolve(root, input));
  if (!isInside(root, path))
    throw new Error(
      "Extension paths must stay inside the package, including symlinks",
    );
  const info = await stat(path);
  if (directory ? !info.isDirectory() : !info.isFile()) {
    throw new Error(
      `Extension path must be a ${directory ? "directory" : "file"}: ${input}`,
    );
  }
  return path;
}

async function checkWitDirectory(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.isDirectory())
      await checkWitDirectory(join(directory, entry.name));
    else if (!entry.isFile() || !entry.name.endsWith(".wit")) {
      throw new Error(
        "Extension wit directories must contain only WIT files and directories, without symlinks",
      );
    }
  }
}

async function manifestPath(config, name) {
  if (isLocalExtension(name)) return join(config.root, name, MANIFEST);
  try {
    return createRequire(join(config.root, "package.json")).resolve(
      `${name}/${MANIFEST}`,
    );
  } catch (error) {
    throw new Error(
      `Cannot resolve ${name}/${MANIFEST}. Install the extension in this application; the package must export its manifest.`,
      { cause: error },
    );
  }
}

async function readExtension(config, name) {
  try {
    const path = await manifestPath(config, name);
    const root = await realpath(dirname(path));
    if ((await stat(path)).size > 65536)
      throw new Error("Manifest exceeds 64 KiB");
    let json;
    try {
      json = JSON.parse(await readFile(path, "utf8"));
    } catch (error) {
      throw new Error("Manifest must be valid JSON", { cause: error });
    }
    const manifest = validateManifest(json, {
      javascript: Boolean(config.main),
    });
    const aliases = [];
    for (const [alias, input] of Object.entries(manifest.aliases)) {
      aliases.push([alias, await packagePath(root, input)]);
    }
    const resolved = {
      name,
      aliases: Object.fromEntries(aliases),
      preload: [],
      components: [],
      imports: manifest.imports,
      permissions: manifest.permissions,
    };
    for (const field of ["preload", "components"]) {
      for (const input of manifest[field])
        resolved[field].push(await packagePath(root, input));
    }
    if (manifest.wit) {
      resolved.wit = await packagePath(root, manifest.wit, true);
      await checkWitDirectory(resolved.wit);
    }
    return resolved;
  } catch (error) {
    throw new Error(`Extension ${name}: ${error.message}`, { cause: error });
  }
}

// The build plan is separate from user configuration. Resolved absolute paths
// and generated WIT never overwrite the user's extension references.
export async function resolveExtensions(config, { mode = "build" } = {}) {
  validateExtensionList(config.extensions);
  const plan = {
    aliases: {},
    preload: [],
    components: [],
    imports: [],
    permissions: [],
    witDirectories: [],
  };
  const aliases = new Map();
  const imports = new Map();
  const permissions = new Set();
  for (const reference of config.extensions || []) {
    const extension = await readExtension(config, reference);
    for (const [name, path] of Object.entries(extension.aliases)) {
      if (aliases.has(name))
        throw new Error(
          `Extension alias conflict for ${name}: ${aliases.get(name).owner} and ${reference}. Select one implementation.`,
        );
      aliases.set(name, { owner: reference, path });
    }
    for (const name of extension.imports) {
      if (imports.has(name))
        throw new Error(
          `Extension import conflict for ${name}: ${imports.get(name)} and ${reference}`,
        );
      imports.set(name, reference);
    }
    plan.preload.push(...extension.preload);
    plan.components.push(...extension.components);
    if (extension.wit) plan.witDirectories.push(extension.wit);
    for (const permission of extension.permissions) permissions.add(permission);
  }
  plan.aliases = Object.fromEntries(
    [...aliases].map(([name, value]) => [name, value.path]),
  );
  plan.imports = [...imports.keys()];
  plan.permissions = [...permissions];
  if (mode === "dev" && permissions.has("outbound-network")) {
    throw new Error(
      "This extension requires outbound-network, which hibana dev does not currently allow. Use a deployment with administrator-approved destinations.",
    );
  }
  return plan;
}

// Generate the one supported HTTP world in disposable build staging only.
export async function prepareExtensionWit(plan, work) {
  if (!plan.witDirectories.length) return undefined;
  const wit = join(work, "wit");
  await mkdir(join(wit, "deps"), { recursive: true });
  await cp(new URL("../wit/deps/", import.meta.url), join(wit, "deps"), {
    recursive: true,
  });
  for (const [index, directory] of plan.witDirectories.entries()) {
    await cp(directory, join(wit, "deps", `extension-${index}`), {
      recursive: true,
      verbatimSymlinks: true,
    });
  }
  const imports = plan.imports.map((name) => `  import ${name};`).join("\n");
  await writeFile(
    join(wit, "world.wit"),
    `package hibana:application;\nworld http {\n${imports}\n  export ${HTTP_CONTRACT};\n}\n`,
  );
  return wit;
}
