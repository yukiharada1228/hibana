import { access, chmod, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { packageInfo, releaseBase } from "./package.mjs";

const binaryLimit = 160 * 1024 * 1024;
export function runtimeTarget(platform = process.platform, arch = process.arch) {
  if (!["linux", "darwin"].includes(platform) || !["x64", "arm64"].includes(arch)) throw new Error(`No local Hibana runtime for ${platform}/${arch}; remote deployment is still available`);
  return `${platform}-${arch}`;
}
function checkedVersion(version) {
  if (!/^\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?$/.test(version)) throw new Error("Runtime version must be a release version such as 0.1.0");
  return version;
}
export function runtimePath(version, { home = process.env.HIBANA_RUNTIME_HOME || join(process.env.XDG_DATA_HOME || join(homedir(), ".local/share"), "hibana/runtimes"), target = runtimeTarget() } = {}) {
  return join(resolve(home), checkedVersion(version), target, "hibana-worker");
}
export async function installedRuntime() {
  const { version } = await packageInfo();
  const path = runtimePath(version);
  try { await access(path, constants.X_OK); return path; }
  catch (error) { if (error.code === "ENOENT" || error.code === "EACCES") return undefined; throw error; }
}
export function releaseChecksum(contents, name) {
  const entries = contents.split(/\r?\n/).map(line => /^([a-f0-9]{64})\s+\*?([^\s]+)$/.exec(line)).filter(Boolean).filter(match => match[2] === name);
  if (entries.length !== 1) throw new Error(`Release checksum missing or duplicated for ${name}`);
  return entries[0][1];
}
async function download(url, limit, signal, fetcher) {
  for (let redirects = 0; redirects <= 5; redirects++) {
    const target = new URL(url);
    if (target.protocol !== "https:" || target.username || target.password) throw new Error("Runtime downloads require HTTPS without URL credentials");
    const response = await fetcher(target, { redirect: "manual", signal });
    if ([301, 302, 303, 307, 308].includes(response.status)) {
      await response.body?.cancel();
      const location = response.headers.get("location");
      if (!location) throw new Error("Runtime download redirect has no destination");
      url = new URL(location, target).href;
      continue;
    }
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(`Runtime release download: HTTP ${response.status}. This version may not be published yet; use --from FILE --sha256 HASH for offline installation`);
    }
    if (Number(response.headers.get("content-length")) > limit) {
      await response.body?.cancel(); throw new Error("Runtime download exceeds size limit");
    }
    const chunks = []; let size = 0;
    for await (const chunk of response.body) {
      size += chunk.length;
      if (size > limit) throw new Error("Runtime download exceeds size limit");
      chunks.push(chunk);
    }
    return Buffer.concat(chunks);
  }
  throw new Error("Too many runtime download redirects");
}

export async function installRuntime(options = {}, { fetcher = fetch, home, target = runtimeTarget() } = {}) {
  const version = checkedVersion(options.version || (await packageInfo()).version);
  if (Boolean(options.from) !== Boolean(options.sha256)) throw new Error("Offline installation requires both --from FILE and --sha256 HASH");
  const name = `hibana-worker-${version}-${target}`;
  let bytes, expected;
  if (options.from) {
    expected = options.sha256;
    if (!/^[a-f0-9]{64}$/.test(expected)) throw new Error("--sha256 must contain 64 lowercase hexadecimal characters");
    const { stat } = await import("node:fs/promises");
    if ((await stat(options.from)).size > binaryLimit) throw new Error("Runtime file exceeds size limit");
    bytes = await readFile(options.from);
  } else {
    const signal = AbortSignal.timeout(180000);
    const base = releaseBase(version);
    expected = releaseChecksum((await download(base + "SHA256SUMS", 64 * 1024, signal, fetcher)).toString("utf8"), name);
    bytes = await download(base + name, binaryLimit, signal, fetcher);
  }
  if (!bytes.length || bytes.length > binaryLimit || createHash("sha256").update(bytes).digest("hex") !== expected) throw new Error("Runtime checksum mismatch; existing installation was preserved");
  const destination = runtimePath(version, { home, target });
  const directory = resolve(destination, "..");
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const temporary = `${destination}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, bytes, { flag: "wx", mode: 0o700 });
    await chmod(temporary, 0o700);
    await rename(temporary, destination);
  } finally { await rm(temporary, { force: true }); }
  console.log(`Installed hibana-worker ${version} (${target}) at ${destination}`);
  return destination;
}
