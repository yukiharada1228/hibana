// Compile a JavaScript fetch handler into a WASI HTTP Component.
import { build as esbuild } from "esbuild";
import { run } from "./process.mjs";
import { mkdtemp, mkdir, rm, writeFile, rename } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
const SDK_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export async function buildComponent({ entry, out }) {
  const entryAbs = resolve(entry), outAbs = resolve(out);
  await mkdir(dirname(outAbs), { recursive: true });
  const work = await mkdtemp(join(tmpdir(), "hibana-build-"));
  const temporary = outAbs + ".building";
  try {
    const shim = join(work, "entry.mjs");
    await writeFile(shim, `
import app from ${JSON.stringify(entryAbs)};
if (!app || typeof app.fetch !== "function") throw new Error("Default export must expose fetch(request, env, context); a Hono app can be exported directly");
function decodeEnv(value) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  const bytes = []; let bits = 0, buffer = 0;
  for (const c of value) {
    const n = alphabet.indexOf(c); if (n < 0) throw new Error("Invalid host environment");
    buffer = (buffer << 6) | n; bits += 6;
    if (bits >= 8) { bits -= 8; bytes.push((buffer >> bits) & 255); }
  }
  return JSON.parse(new TextDecoder().decode(new Uint8Array(bytes)));
}
addEventListener("fetch", event => {
  event.respondWith((async () => {
    const incoming = event.request;
    const raw = incoming.headers.get("x-hibana-env");
    const env = raw ? decodeEnv(raw) : {};
    const headers = new Headers(incoming.headers);
    headers.delete("x-hibana-env"); headers.delete("x-hibana-event");
    const body = incoming.method === "GET" || incoming.method === "HEAD" ? undefined : await incoming.arrayBuffer();
    const request = new Request(incoming.url, { method: incoming.method, headers, body });
    const context = { waitUntil(promise) { event.waitUntil(Promise.resolve(promise)); } };
    return app.fetch(request, env, context);
  })());
});
`);
    const bundle = join(work, "worker.mjs");
    await esbuild({ entryPoints: [shim], bundle: true, format: "esm", platform: "browser", target: "es2022", mainFields: ["module", "main"], conditions: ["import", "default"], outfile: bundle, logLevel: "warning" });
    await run(process.execPath, [join(SDK_ROOT, "node_modules/.bin/jco"), "componentize", bundle, "--wit", join(SDK_ROOT, "wit"), "--world-name", "http", "--out", temporary]);
    await rename(temporary, outAbs);
    return outAbs;
  } finally {
    await rm(work, { recursive: true, force: true });
    await rm(temporary, { force: true });
  }
}
