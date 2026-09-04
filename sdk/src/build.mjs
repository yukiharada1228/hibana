// Hibana ビルドパイプライン (M11-5, §4.2)
//
// ユーザーの Hono アプリ（`export default app`）を **wasi:http/incoming-handler** を export する
// WebAssembly Component へ変換する。エンベロープ変換用の adapter は不要 —— StarlingMonkey が
// `fetch` イベントを incoming-handler へ配線し、Hono の `app.fetch(Request)->Response` がそのまま動く。
//
//   1. esbuild で「fetch-event shim + ユーザーアプリ + Hono」を 1 本の ESM にバンドル
//   2. jco componentize（--disable http で outgoing-handler=egress を落とす。incoming は残る）
//
// 版の注意（重要）: host（worker / wasmtime-wasi-http 29）は wasi:http@0.2.3。
// jco は wasi:http < 0.2.10 の world を検出すると componentize-js 0.19.3 に fallback し、
// incoming-handler を正しく生やす。0.2.10/0.2.12 を target すると生成に失敗する。
// このため SDK は 0.2.3 の WIT（sdk/wit）を同梱している。
//
// 使い方:
//   node src/build.mjs --entry examples/hello.ts --out dist/hello.wasm
//
import { build as esbuild } from "esbuild";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SDK_ROOT = resolve(__dirname, "..");
// SDK 同梱の 0.2.3 WIT（incoming-handler world + wasi deps）。
const DEFAULT_WIT = resolve(SDK_ROOT, "wit");

function parseArgs(argv) {
  const out = { wit: DEFAULT_WIT, world: "http" };
  for (let i = 0; i < argv.length; i += 2) {
    const k = argv[i];
    const v = argv[i + 1];
    if (k === "--entry") out.entry = v;
    else if (k === "--out") out.out = v;
    else if (k === "--wit") out.wit = v;
    else if (k === "--world") out.world = v;
    else throw new Error(`unknown arg: ${k}`);
  }
  if (!out.entry) throw new Error("--entry <path> is required");
  if (!out.out) throw new Error("--out <component.wasm> is required");
  return out;
}

function run(cmd, args) {
  return new Promise((res, rej) => {
    const p = spawn(cmd, args, { stdio: ["ignore", "inherit", "inherit"] });
    p.on("error", rej);
    p.on("close", (code) =>
      code === 0 ? res() : rej(new Error(`${cmd} exited ${code}`)),
    );
  });
}

export async function buildComponent({ entry, out, wit, world }) {
  wit = wit || DEFAULT_WIT;
  world = world || "http";
  const entryAbs = resolve(process.cwd(), entry);
  const outAbs = resolve(process.cwd(), out);
  const witAbs = resolve(process.cwd(), wit);
  await mkdir(dirname(outAbs), { recursive: true });

  const work = await mkdtemp(join(tmpdir(), "hibana-build-"));
  try {
    // 1. fetch-event shim を生成。StarlingMonkey がこの fetch リスナを
    //    wasi:http/incoming-handler へ配線する（adapter は不要）。
    const shim = join(work, "entry.mjs");
    await writeFile(
      shim,
      [
        `import _app from ${JSON.stringify(entryAbs)};`,
        // default export の相互運用（app / {default: app} / {fetch} を吸収）。
        `const app = _app && _app.fetch ? _app : (_app && _app.default) ? _app.default : _app;`,
        `if (!app || typeof app.fetch !== "function") {`,
        `  throw new Error("entry must \`export default\` a Hono app (needs .fetch)");`,
        `}`,
        `addEventListener("fetch", (event) => event.respondWith(app.fetch(event.request)));`,
        "",
      ].join("\n"),
    );

    // 2. esbuild で 1 本の ESM にバンドル。
    const bundle = join(work, "bundle.js");
    await esbuild({
      entryPoints: [shim],
      bundle: true,
      format: "esm",
      platform: "neutral",
      target: "es2022",
      mainFields: ["module", "main"],
      conditions: ["import", "default"],
      outfile: bundle,
      logLevel: "warning",
    });

    // 3. jco componentize（--disable http: egress を落とす。incoming は残る）。
    const jco = resolve(SDK_ROOT, "node_modules", ".bin", "jco");
    await run(jco, [
      "componentize",
      bundle,
      "--wit",
      witAbs,
      "--world-name",
      world,
      "--disable",
      "http",
      "--out",
      outAbs,
    ]);

    return outAbs;
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

// CLI エントリ
if (import.meta.url === `file://${process.argv[1]}`) {
  buildComponent(parseArgs(process.argv.slice(2)))
    .then((p) => console.log(`✓ Component: ${p}`))
    .catch((e) => {
      console.error(e.message || e);
      process.exit(1);
    });
}
