import { readFile } from "node:fs/promises";

export async function packageInfo() {
  return JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
}

export function releaseBase(version) {
  if (!/^\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?$/.test(version)) throw new Error("Version must be a release version such as 0.1.0");
  return `https://github.com/yukiharada1228/hibana/releases/download/v${version}/`;
}

export function cliRelease(version) {
  return `${releaseBase(version)}hibana-cli-${version}.tgz`;
}
