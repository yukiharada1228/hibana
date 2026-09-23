// Compile a JavaScript fetch handler into a WASI HTTP Component.
import { build as esbuild } from "esbuild";
import { run } from "./process.mjs";
import { extensionImports } from "./extension-imports.mjs";
import { writeFile } from "node:fs/promises";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
const SDK_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// build.mjs owns staging, cleanup and atomic publication for every language.
export async function compileJavaScript(config, out, extensions = {}) {
  const work = dirname(out);
  const shim = join(work, "entry.mjs");
  await writeFile(
    shim,
    `
${(extensions.preload || []).map((path) => `import ${JSON.stringify(path)};`).join("\n")}
import app from ${JSON.stringify(resolve(config.root, config.main))};
if (!app || typeof app.fetch !== "function") throw new Error("Default export must expose fetch(request, env, context); a Hono app can be exported directly");
function decodeEnv(value) {
  const bytes = Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/")), c => c.charCodeAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
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
`,
  );
  const bundle = join(work, "worker.mjs");
  await esbuild({
    entryPoints: [shim],
    bundle: true,
    format: "esm",
    platform: "browser",
    target: "es2022",
    mainFields: ["module", "main"],
    conditions: ["import", "default"],
    outfile: bundle,
    logLevel: "warning",
    absWorkingDir: config.root,
    alias: extensions.aliases || {},
    external: extensions.imports || [],
    plugins: [extensionImports(extensions.packages)],
  });
  const wit = extensions.wit || join(SDK_ROOT, "wit");
  await run(
    process.execPath,
    [
      fileURLToPath(
        new URL("./jco.js", import.meta.resolve("@bytecodealliance/jco")),
      ),
      "componentize",
      bundle,
      "--wit",
      wit,
      "--world-name",
      "http",
      "--out",
      out,
    ],
    { signal: extensions.signal },
  );
}
