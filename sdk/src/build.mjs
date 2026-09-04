// Hibana ビルドパイプライン (M11-2, §4.2)
//
// ユーザーの Hono アプリ（`export default app`）を WebAssembly Component へ変換する。
//   1. esbuild で「adapter + ユーザーアプリ + Hono」を 1 本の ESM にバンドル
//   2. jco componentize で Component 化（--disable http で egress=outgoing-handler を外す。
//      wasi:http/types は残り、Request/Response はそれで動く）
//
// 生成物 (`<out>.wasm`) の world は platform の faas:component/handler:
//   handle: func(input: list<u8>) -> result<list<u8>, handler-error>
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
const DEFAULT_WIT = resolve(SDK_ROOT, "..", "wit", "world.wit");
const ADAPTER = resolve(__dirname, "adapter.js");

function parseArgs(argv) {
  const out = { wit: DEFAULT_WIT, world: "handler" };
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
  world = world || "handler";
  const entryAbs = resolve(process.cwd(), entry);
  const outAbs = resolve(process.cwd(), out);
  const witAbs = resolve(process.cwd(), wit);
  await mkdir(dirname(outAbs), { recursive: true });

  const work = await mkdtemp(join(tmpdir(), "hibana-build-"));
  try {
    // 1. handle を生やすエントリを生成（adapter がユーザー app を包む）。
    const shim = join(work, "entry.mjs");
    await writeFile(
      shim,
      [
        `import app from ${JSON.stringify(entryAbs)};`,
        `import { createHandle } from ${JSON.stringify(ADAPTER)};`,
        `export const handle = createHandle(app.fetch ? app : (app.default ?? app));`,
        "",
      ].join("\n"),
    );

    // 2. esbuild で 1 本の ESM にバンドル（jco componentize は単一ファイルを食う）。
    const bundle = join(work, "bundle.js");
    await esbuild({
      entryPoints: [shim],
      bundle: true,
      format: "esm",
      platform: "neutral",
      // StarlingMonkey は最近の JS を解釈できる。過度な down-level は不要。
      target: "es2022",
      // 外部 I/O は Component 側の import に無いため、Node 組み込みは解決しない。
      // ユーザーコードがそれらを使っていればここで失敗させる（実行時謎エラーより親切）。
      mainFields: ["module", "main"],
      conditions: ["import", "default"],
      outfile: bundle,
      logLevel: "warning",
    });

    // 3. jco componentize（--disable http: egress を外す。types は残る）。
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
