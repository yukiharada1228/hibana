// Installations are published whole when new. Repairs replace individual files
// atomically, so another build never loses an already complete cache directory.
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  rename,
  writeFile,
} from "node:fs/promises";
import { dirname, join, isAbsolute } from "node:path";

async function inventory(root, directory = "") {
  const files = [];
  for (const entry of await readdir(join(root, directory), {
    withFileTypes: true,
  })) {
    const path = directory ? `${directory}/${entry.name}` : entry.name;
    if (entry.isDirectory()) files.push(...(await inventory(root, path)));
    else if (entry.isFile())
      files.push([path, (await lstat(join(root, path))).size]);
    else
      throw new Error(
        "Extension caches must contain regular files and directories",
      );
  }
  return files;
}

export async function isCompleteInstallation(root, key) {
  try {
    if (!(await lstat(root)).isDirectory()) return false;
    const ready = JSON.parse(await readFile(join(root, "ready"), "utf8"));
    if (
      ready?.key !== key ||
      !Array.isArray(ready.files) ||
      !ready.files.length
    )
      return false;
    const directories = new Set([root]);
    for (const file of ready.files) {
      if (!Array.isArray(file) || file.length !== 2) return false;
      const [path, size] = file;
      if (
        typeof path !== "string" ||
        !path ||
        isAbsolute(path) ||
        path.includes("\0") ||
        path.includes("\\") ||
        path
          .split("/")
          .some((part) => !part || part === "." || part === "..") ||
        !Number.isSafeInteger(size) ||
        size < 0
      )
        return false;
      const target = join(root, path);
      for (
        let directory = dirname(target);
        !directories.has(directory);
        directory = dirname(directory)
      ) {
        if (!(await lstat(directory)).isDirectory()) return false;
        directories.add(directory);
      }
      const info = await lstat(target);
      if (!info.isFile() || info.size !== size) return false;
    }
    return true;
  } catch (error) {
    if (
      error instanceof SyntaxError ||
      ["ENOENT", "ENOTDIR", "EISDIR"].includes(error.code)
    )
      return false;
    throw error;
  }
}

export async function publishInstallation(staging, root, key) {
  const files = await inventory(staging);
  await writeFile(
    join(staging, "ready"),
    JSON.stringify({ key, files }) + "\n",
  );
  try {
    await rename(staging, root);
    return;
  } catch (error) {
    if (!["EEXIST", "ENOTEMPTY"].includes(error.code)) throw error;
  }
  if (await isCompleteInstallation(root, key)) return;

  // A partial restore can retain the old ready marker. Do not discard this
  // verified installation just because its destination directory already exists.
  const directories = new Set();
  async function ensureDirectory(path) {
    if (directories.has(path)) return;
    if (path !== root) await ensureDirectory(dirname(path));
    try {
      await mkdir(path);
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
    }
    if (!(await lstat(path)).isDirectory())
      throw new Error(`Invalid extension cache directory: ${path}`);
    directories.add(path);
  }
  for (const [path] of files) {
    const target = join(root, path);
    await ensureDirectory(dirname(target));
    await rename(join(staging, path), target);
  }
  await rename(join(staging, "ready"), join(root, "ready"));
}
