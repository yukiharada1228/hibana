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

export async function buildComponent({ entry, out, wit, world, kv, r2 }) {
  wit = wit || DEFAULT_WIT;
  world = world || "http";
  // kv: [{ binding, id }]（wrangler の kv_namespaces）。binding→namespace を shim に焼き込む。
  const kvBindings = Array.isArray(kv) ? kv : [];
  // r2: [{ binding, bucket_name }]（wrangler の r2_buckets）。
  const r2Bindings = Array.isArray(r2) ? r2 : [];
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
        // M11-9: worker が注入する x-hibana-env（base64url(JSON) の config/secret マップ）を
        // 復号して読み口を用意する。StarlingMonkey は wasi:cli/environment を JS へ出さないため、
        // env はこのヘッダ経由で受け取る。app.fetch の第2引数（Hono の c.env, Workers 流儀）と
        // globalThis.process.env の両方に載せ、読んだヘッダは app へ渡す前に剥がす。
        `const __B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";`,
        `function __b64urlToString(str) {`,
        `  const s = String(str).replace(/[^A-Za-z0-9\\-_]/g, ""); const b = [];`,
        `  for (let i = 0; i < s.length; i += 4) {`,
        `    const c0 = __B64.indexOf(s[i]), c1 = __B64.indexOf(s[i+1]), c2 = __B64.indexOf(s[i+2]), c3 = __B64.indexOf(s[i+3]);`,
        `    b.push((c0 << 2) | (c1 >> 4));`,
        `    if (i + 2 < s.length && c2 >= 0) b.push(((c1 & 15) << 4) | (c2 >> 2));`,
        `    if (i + 3 < s.length && c3 >= 0) b.push(((c2 & 3) << 6) | c3);`,
        `  }`,
        `  return new TextDecoder().decode(new Uint8Array(b));`,
        `}`,
        `addEventListener("fetch", (event) => {`,
        `  let request = event.request;`,
        `  let env = {};`,
        `  const raw = request.headers.get("x-hibana-env");`,
        `  if (raw) {`,
        `    try { env = JSON.parse(__b64urlToString(raw)) || {}; } catch { env = {}; }`,
        `    const h = new Headers(request.headers); h.delete("x-hibana-env");`,
        `    request = new Request(request, { headers: h });`,
        `  }`,
        `  globalThis.process = globalThis.process || {};`,
        `  globalThis.process.env = env;`,
        // M12: KV バインディング（Workers KV 互換）。env.<binding> = kv.hibana.internal への client。
        // worker の send_request が egress せず Postgres で処理する（tenant はジョブ由来）。
        `  const __KV = ${JSON.stringify(kvBindings.map((b) => [b.binding, b.id || b.binding]))};`,
        `  for (const [__b, __ns] of __KV) env[__b] = __kvClient(__ns);`,
        `  const __R2 = ${JSON.stringify(r2Bindings.map((b) => [b.binding, b.bucket_name || b.binding]))};`,
        `  for (const [__b, __bk] of __R2) env[__b] = __r2Client(__bk);`,
        // Workers 互換: fetch(request, env, ctx)。ctx は no-op stub（waitUntil/passThroughOnException）。
        `  const ctx = { waitUntil() {}, passThroughOnException() {} };`,
        `  event.respondWith(app.fetch(request, env, ctx));`,
        `});`,
        // Workers KV 互換の最小 client（get/put/delete/list）。
        `function __kvClient(ns) {`,
        `  const base = "http://kv.hibana.internal/v1/kv";`,
        `  const e = encodeURIComponent;`,
        `  return {`,
        `    async get(key, type) {`,
        `      const r = await fetch(base + "?ns=" + e(ns) + "&key=" + e(key));`,
        `      if (r.status === 404) return null;`,
        `      if (!r.ok) throw new Error("KV get failed: " + r.status);`,
        `      if (type === "json") return await r.json();`,
        `      if (type === "arrayBuffer") return await r.arrayBuffer();`,
        `      return await r.text();`,
        `    },`,
        `    async put(key, value, opts) {`,
        `      const ttl = opts && opts.expirationTtl ? "&ttl=" + opts.expirationTtl : "";`,
        `      const body = (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) ? value : String(value);`,
        `      const r = await fetch(base + "?ns=" + e(ns) + "&key=" + e(key) + ttl, { method: "PUT", body });`,
        `      if (!r.ok) throw new Error("KV put failed: " + r.status);`,
        `    },`,
        `    async delete(key) {`,
        `      const r = await fetch(base + "?ns=" + e(ns) + "&key=" + e(key), { method: "DELETE" });`,
        `      if (!r.ok) throw new Error("KV delete failed: " + r.status);`,
        `    },`,
        `    async list(opts) {`,
        `      const p = opts && opts.prefix ? "&prefix=" + e(opts.prefix) : "";`,
        `      const l = opts && opts.limit ? "&limit=" + opts.limit : "";`,
        `      const r = await fetch(base + "/list?ns=" + e(ns) + p + l);`,
        `      if (!r.ok) throw new Error("KV list failed: " + r.status);`,
        `      const j = await r.json();`,
        `      return { keys: (j.keys || []).map((name) => ({ name })), list_complete: true, cursor: "" };`,
        `    },`,
        `  };`,
        `}`,
        // base64url encode（x-r2-meta 用。カスタムメタは任意文字を含むためヘッダに base64url で載せる）。
        `const __B64E = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";`,
        `function __b64urlEnc(str) {`,
        `  const b = new TextEncoder().encode(str); let o = "";`,
        `  for (let i = 0; i < b.length; i += 3) {`,
        `    const b0 = b[i], b1 = i+1 < b.length ? b[i+1] : 0, b2 = i+2 < b.length ? b[i+2] : 0;`,
        `    o += __B64E[b0 >> 2]; o += __B64E[((b0 & 3) << 4) | (b1 >> 4)];`,
        `    if (i+1 < b.length) o += __B64E[((b1 & 15) << 2) | (b2 >> 6)];`,
        `    if (i+2 < b.length) o += __B64E[b2 & 63];`,
        `  }`,
        `  return o;`,
        `}`,
        // Workers R2 互換の最小 client（put/get/head/delete/list）。
        `function __r2meta(h) {`,
        `  let cm = {}; try { const m = h.get("x-r2-meta"); if (m) cm = JSON.parse(__b64urlToString(m)); } catch {}`,
        `  return { key: h.get("x-r2-key"), size: Number(h.get("x-r2-size") || 0), etag: h.get("x-r2-etag"),`,
        `           httpMetadata: { contentType: h.get("content-type") || undefined }, customMetadata: cm };`,
        `}`,
        `function __r2Client(bucket) {`,
        `  const base = "http://r2.hibana.internal/v1/r2";`,
        `  const e = encodeURIComponent;`,
        `  return {`,
        `    async put(key, value, opts) {`,
        `      const headers = {};`,
        `      if (opts && opts.httpMetadata && opts.httpMetadata.contentType) headers["content-type"] = opts.httpMetadata.contentType;`,
        `      if (opts && opts.customMetadata) headers["x-r2-meta"] = __b64urlEnc(JSON.stringify(opts.customMetadata));`,
        `      const b = (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) ? value : String(value);`,
        `      const r = await fetch(base + "?bucket=" + e(bucket) + "&key=" + e(key), { method: "PUT", headers, body: b });`,
        `      if (!r.ok) throw new Error("R2 put failed: " + r.status);`,
        `      return __r2meta(r.headers);`,
        `    },`,
        `    async get(key) {`,
        `      const r = await fetch(base + "?bucket=" + e(bucket) + "&key=" + e(key));`,
        `      if (r.status === 404) return null;`,
        `      if (!r.ok) throw new Error("R2 get failed: " + r.status);`,
        `      const buf = await r.arrayBuffer(); const meta = __r2meta(r.headers);`,
        `      return Object.assign(meta, { body: buf, arrayBuffer: async () => buf,`,
        `        text: async () => new TextDecoder().decode(buf),`,
        `        json: async () => JSON.parse(new TextDecoder().decode(buf)) });`,
        `    },`,
        `    async head(key) {`,
        `      const r = await fetch(base + "?bucket=" + e(bucket) + "&key=" + e(key) + "&head=1");`,
        `      if (r.status === 404) return null; if (!r.ok) throw new Error("R2 head failed: " + r.status);`,
        `      return __r2meta(r.headers);`,
        `    },`,
        `    async delete(key) {`,
        `      const r = await fetch(base + "?bucket=" + e(bucket) + "&key=" + e(key), { method: "DELETE" });`,
        `      if (!r.ok && r.status !== 404) throw new Error("R2 delete failed: " + r.status);`,
        `    },`,
        `    async list(opts) {`,
        `      const p = opts && opts.prefix ? "&prefix=" + e(opts.prefix) : "";`,
        `      const l = opts && opts.limit ? "&limit=" + opts.limit : "";`,
        `      const r = await fetch(base + "/list?bucket=" + e(bucket) + p + l);`,
        `      if (!r.ok) throw new Error("R2 list failed: " + r.status);`,
        `      const j = await r.json();`,
        `      return { objects: (j.objects || []).map((o) => ({ key: o.key, size: o.size, etag: o.etag })), truncated: false };`,
        `    },`,
        `  };`,
        `}`,
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

    // 3. jco componentize。
    //    M11-8 (§4.4): `--disable http` は付けない —— outgoing-handler（guest の fetch）を残す。
    //    egress の抑止はビルド時ではなく **runtime の allowlist**（worker の send_request gate）で
    //    行う。allowlist が空なら全拒否なので、既定では従来どおり fetch は不可（deny-by-default）。
    const jco = resolve(SDK_ROOT, "node_modules", ".bin", "jco");
    await run(jco, [
      "componentize",
      bundle,
      "--wit",
      witAbs,
      "--world-name",
      world,
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
