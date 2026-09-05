#!/usr/bin/env node
// hibana CLI (M11-3, §4.2 / §15)
//
// Hono アプリを WebAssembly Component にして自前基盤へ運ぶ開発者体験。
//
//   hibana deploy [--entry src/index.ts] [--name <app>] [--version 0.1.0]
//   hibana dev    [--entry src/index.ts] [--port 8787]
//   hibana invoke <app> [METHOD] [PATH] [--body '<str>'] [--header k:v]
//   hibana logs   <execution_id>
//   hibana secret set <app> <NAME> <VALUE>
//   hibana rollback <app> [version]
//
// 接続情報は env で与える（未設定はローカル dev 既定）:
//   HIBANA_URL       (default http://localhost:8080)
//   HIBANA_TOKEN     直接トークンを渡す場合（tenant/email/password より優先）
//   HIBANA_TENANT    (default smoke)
//   HIBANA_EMAIL     (default admin@example.com)
//   HIBANA_PASSWORD  (default dev-password)
import { buildComponent } from "./build.mjs";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

// ---- 出力ヘルパ -----------------------------------------------------------
const green = (s) => `\x1b[32m${s}\x1b[0m`;
const dim = (s) => `\x1b[2m${s}\x1b[0m`;
const bold = (s) => `\x1b[1m${s}\x1b[0m`;
const red = (s) => `\x1b[31m${s}\x1b[0m`;
const ok = (s) => console.log(`${green("✓")} ${s}`);
const step = (s) => process.stdout.write(`${dim("•")} ${s} ... `);
const stepDone = () => process.stdout.write(`${green("done")}\n`);
function die(msg) {
  console.error(`${red("✗")} ${msg}`);
  process.exit(1);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// 実行が未終端か（pending も running も「まだ」）。終端は succeeded/failed/timeout。
const nonTerminal = (s) => s === "pending" || s === "running";

// ---- 設定 / 認証 ----------------------------------------------------------
function config() {
  return {
    url: (process.env.HIBANA_URL || "http://localhost:8080").replace(/\/$/, ""),
    token: process.env.HIBANA_TOKEN || null,
    tenant: process.env.HIBANA_TENANT || "smoke",
    email: process.env.HIBANA_EMAIL || "admin@example.com",
    password: process.env.HIBANA_PASSWORD || "dev-password",
    // 公開 ingress のベースドメイン（サーバの INGRESS_BASE_DOMAIN と一致させる）。
    // 設定時のみ deploy --public / publish が公開 URL を表示する。
    ingressDomain: process.env.HIBANA_INGRESS_DOMAIN || null,
  };
}

// 公開 URL（<app>.<tenant>.<base>）。ベースドメイン未設定なら null。
function ingressUrl(cfg, name) {
  return cfg.ingressDomain
    ? `http://${name}.${cfg.tenant}.${cfg.ingressDomain}/`
    : null;
}

async function setIngress(cfg, token, name, enabled) {
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  await api(cfg, "PUT", `/components/${id}/ingress`, {
    token,
    json: { enabled },
  });
  return id;
}

// 初回 invoke を打って worker に component を precompile・キャッシュさせる（best-effort）。
async function warmUp(cfg, token, name) {
  try {
    let r = await api(cfg, "POST", "/invoke?wait=1", {
      token,
      json: { component: name, input: { method: "GET", path: "/" } },
    });
    if (nonTerminal(r.status) && r.execution_id) {
      const started = Date.now();
      while (nonTerminal(r.status) && Date.now() - started < 90_000) {
        await sleep(600);
        r = await api(cfg, "GET", `/executions/${r.execution_id}`, { token });
      }
    }
  } catch {
    // warm-up の失敗は致命ではない（初回ヒットが cold になるだけ）。
  }
}

// ---- Workers 互換: wrangler.toml / wrangler.jsonc をプロジェクト設定として読む ----------
function stripJsonComments(s) {
  // ブロックコメントと行コメント（http:// のような `:` 直後の // は残す簡易ヒューリスティック）。
  return s
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1");
}

async function loadProjectConfig() {
  const files = [
    "wrangler.toml",
    "wrangler.jsonc",
    "wrangler.json",
    "hibana.toml",
    "hibana.jsonc",
    "hibana.json",
  ];
  for (const f of files) {
    let text;
    try {
      text = await readFile(f, "utf8");
    } catch {
      continue;
    }
    let raw;
    try {
      if (f.endsWith(".toml")) {
        const { parse } = await import("smol-toml");
        raw = parse(text);
      } else {
        raw = JSON.parse(stripJsonComments(text));
      }
    } catch (e) {
      die(`failed to parse ${f}: ${e.message || e}`);
    }
    const h = raw.hibana || {};
    return {
      file: f,
      name: raw.name,
      main: raw.main, // Workers: entry module
      vars: raw.vars && typeof raw.vars === "object" ? raw.vars : {},
      // hibana 固有（wrangler には無い）。
      public: h.public ?? false,
      egress: Array.isArray(h.egress) ? h.egress : [],
      ingressDomain: h.ingress_domain,
    };
  }
  return null;
}

// version の承認 env 名リストに names を **追加**（GET でマージ → PUT 全置換）。replace=true で全置換。
async function grantEnvMerged(cfg, token, id, version, names, replace = false) {
  let existing = [];
  if (!replace) {
    try {
      const cur = await api(
        cfg,
        "GET",
        `/components/${id}/versions/${version}/capabilities`,
        { token },
      );
      existing = (cur && cur.env) || [];
    } catch {}
  }
  const merged = [...new Set([...existing, ...names])];
  await api(cfg, "PUT", `/components/${id}/versions/${version}/capabilities`, {
    token,
    json: { env: merged },
  });
  return merged;
}

// component の現在の active version 文字列を解決する（secret/config の自動 grant 用）。
async function resolveActiveVersion(cfg, token, id) {
  const list = await api(cfg, "GET", "/components", { token });
  const arr = Array.isArray(list) ? list : list.components || [];
  const comp = arr.find((c) => c.component_id === id);
  const activeId = comp && comp.active_version_id;
  if (!activeId) return null;
  try {
    const vs = await api(cfg, "GET", `/components/${id}/versions`, { token });
    const varr = Array.isArray(vs) ? vs : vs.versions || [];
    const hit = varr.find((v) => v.version_id === activeId);
    return hit ? hit.version : null;
  } catch {
    return null;
  }
}

async function api(cfg, method, path, { token, json, form } = {}) {
  const headers = {};
  if (token) headers["authorization"] = `Bearer ${token}`;
  let body;
  if (json !== undefined) {
    headers["content-type"] = "application/json";
    body = JSON.stringify(json);
  } else if (form !== undefined) {
    body = form; // FormData: content-type は fetch が boundary 付きで設定
  }
  const res = await fetch(`${cfg.url}${path}`, { method, headers, body });
  const text = await res.text();
  let parsed;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  if (!res.ok) {
    const m =
      parsed && parsed.error ? parsed.error.message : `HTTP ${res.status}`;
    throw new Error(m);
  }
  return parsed;
}

async function login(cfg) {
  if (cfg.token) return cfg.token;
  const r = await api(cfg, "POST", "/auth/login", {
    json: { tenant_slug: cfg.tenant, email: cfg.email, password: cfg.password },
  });
  if (!r || !r.token) throw new Error("login failed: no token in response");
  return r.token;
}

async function resolveComponentId(cfg, token, name) {
  const list = await api(cfg, "GET", "/components", { token });
  const arr = Array.isArray(list) ? list : list.components || [];
  const hit = arr.find((c) => c.name === name);
  return hit ? hit.component_id : null;
}

async function ensureComponent(cfg, token, name) {
  let id = await resolveComponentId(cfg, token, name);
  if (id) return id;
  await api(cfg, "POST", "/components", { token, json: { name } });
  id = await resolveComponentId(cfg, token, name);
  if (!id) throw new Error(`failed to create component '${name}'`);
  return id;
}

// ---- 引数パース -----------------------------------------------------------
function parseFlags(args) {
  const flags = {};
  const pos = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a.startsWith("--")) {
      const key = a.slice(2);
      const next = args[i + 1];
      if (next === undefined || next.startsWith("--")) flags[key] = true;
      else {
        flags[key] = next;
        i++;
      }
    } else pos.push(a);
  }
  return { flags, pos };
}

// ---- Hono app をローカルにロード（esbuild で TS→ESM して import）-----------
// **本番ビルド（build.mjs）と同じ解決規則**（platform:neutral / conditions）で束ねる。
// こうすると Node 組み込み（node:fs 等）への import は dev でも本番と同様に解決失敗し、
// 「dev では動くが本番でビルドできない」ズレを最小化する（dev を忠実なプレビューにする）。
async function loadApp(entry) {
  const { build } = await import("esbuild");
  const work = await mkdtemp(join(tmpdir(), "hibana-dev-"));
  const outfile = join(work, "app.mjs");
  await build({
    entryPoints: [resolve(process.cwd(), entry)],
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "es2022",
    mainFields: ["module", "main"],
    conditions: ["import", "default"],
    outfile,
    logLevel: "warning",
  });
  const mod = await import(pathToFileURL(outfile).href);
  await rm(work, { recursive: true, force: true });
  const app = mod.default?.fetch ? mod.default : mod.app || mod.default;
  if (!app || typeof app.fetch !== "function")
    throw new Error(`${entry} must \`export default\` a Hono app`);
  return app;
}

// ---- commands -------------------------------------------------------------
async function cmdDeploy(args) {
  const { flags } = parseFlags(args);
  // Workers 互換: フラグが無ければ wrangler.toml / wrangler.jsonc を読む。
  const proj = await loadProjectConfig();
  const entry = flags.entry || proj?.main || "src/index.ts";
  const name = flags.name || proj?.name || "app";
  // Workers 流儀: deploy ごとに新しい版を作る。明示指定が無ければ単調増加する版を採番
  // （固定版だと再 deploy で "version already exists" になる）。semver の patch に unix 秒を使う。
  const version = flags.version || `0.0.${Math.floor(Date.now() / 1000)}`;
  const isPublic = flags.public || proj?.public || false;
  const cfg = config();
  if (proj?.ingressDomain && !cfg.ingressDomain) cfg.ingressDomain = proj.ingressDomain;

  console.log(bold(`\nDeploying ${name} → ${cfg.url}`));
  if (proj) console.log(dim(`  (config: ${proj.file})`));
  console.log("");

  step("Building TypeScript → WebAssembly Component");
  const work = await mkdtemp(join(tmpdir(), "hibana-deploy-"));
  const outWasm = join(work, "component.wasm");
  await buildComponent({ entry, out: outWasm });
  const wasm = await readFile(outWasm);
  await rm(work, { recursive: true, force: true });
  stepDone();

  step("Authenticating");
  const token = await login(cfg);
  const id = await ensureComponent(cfg, token, name);
  // 前 active version の承認 env 名を引き継ぐ（redeploy で secret の grant を失わないため）。
  let inheritedEnv = [];
  const prevVersion = await resolveActiveVersion(cfg, token, id);
  if (prevVersion) {
    try {
      const caps = await api(
        cfg,
        "GET",
        `/components/${id}/versions/${prevVersion}/capabilities`,
        { token },
      );
      inheritedEnv = (caps && caps.env) || [];
    } catch {}
  }
  stepDone();

  step(`Uploading version ${version}`);
  const fd = new FormData();
  fd.set("version", version);
  fd.set(
    "resource_limits",
    JSON.stringify({
      max_wall_time_ms: 15000,
      max_execution_time_ms: 20000,
      max_memory_bytes: 268435456,
    }),
  );
  fd.set("wasm", new Blob([wasm], { type: "application/wasm" }), `${name}.wasm`);
  await api(cfg, "POST", `/components/${id}/versions`, { token, form: fd });
  await api(cfg, "PUT", `/components/${id}/active-version`, {
    token,
    json: { version },
  });
  stepDone();

  // 初回 precompile（十数 MiB）を deploy 中に済ませる。これで公開 URL や invoke の
  // 初回ヒットが cold precompile で待たされない（worker がキャッシュ済みになる）。
  step("Warming up");
  await warmUp(cfg, token, name);
  stepDone();

  // Workers 互換: wrangler.toml の [vars] を config 値としてマージ設定。
  const varNames = proj ? Object.keys(proj.vars) : [];
  if (varNames.length) {
    step(`Applying ${varNames.length} config var(s)`);
    let env = {};
    try {
      const cur = await api(cfg, "GET", `/components/${id}/config`, { token });
      if (cur && cur.env) env = { ...cur.env };
    } catch {}
    env = { ...env, ...proj.vars };
    await api(cfg, "PUT", `/components/${id}/config`, { token, json: { env } });
    stepDone();
  }
  // この version が読める env 名 = 引き継ぎ（前 version の secret 等）∪ 今回の config vars。
  // 承認は per-version なので、新 version にも必ず付け直す（付けないと redeploy で読めなくなる）。
  const grantNames = [...new Set([...inheritedEnv, ...varNames])];
  if (grantNames.length) {
    step(`Approving ${grantNames.length} env name(s)`);
    await grantEnvMerged(cfg, token, id, version, grantNames, true);
    stepDone();
  }

  // hibana 固有: egress allowlist を承認。
  if (proj && proj.egress.length) {
    step(`Approving egress (${proj.egress.length})`);
    await api(cfg, "PUT", `/components/${id}/versions/${version}/capabilities/egress`, {
      token,
      json: { allow_outbound: proj.egress },
    });
    stepDone();
  }

  let published = false;
  if (isPublic) {
    step("Publishing (public ingress)");
    await setIngress(cfg, token, name, true);
    published = true;
    stepDone();
  }

  ok(bold(`Deployed ${name}@${version}`));
  const url = published ? ingressUrl(cfg, name) : null;
  if (url) {
    console.log(`${green("→")} ${bold(url)}`);
    console.log(
      dim(
        `  (Local dev: curl -H "Host: ${name}.${cfg.tenant}.${cfg.ingressDomain}" ${cfg.url}/ )\n`,
      ),
    );
  } else {
    console.log(
      dim(
        `\n  Invoke it:\n    hibana invoke ${name} GET /` +
          (published
            ? `\n  Public ingress is ON (set HIBANA_INGRESS_DOMAIN to print the URL).\n`
            : `\n  Make it public:  hibana publish ${name}\n`),
      ),
    );
  }
}

async function cmdPublish(args, enabled) {
  const { pos } = parseFlags(args);
  const [name] = pos;
  if (!name)
    die(`usage: hibana ${enabled ? "publish" : "unpublish"} <app>`);
  const cfg = config();
  const token = await login(cfg);
  await setIngress(cfg, token, name, enabled);
  if (enabled) {
    const url = ingressUrl(cfg, name);
    ok(`published ${bold(name)}` + (url ? ` → ${bold(url)}` : " (public ingress ON)"));
  } else {
    ok(`unpublished ${bold(name)} (public ingress OFF)`);
  }
}

async function cmdInvoke(args) {
  const { flags, pos } = parseFlags(args);
  const [name, method = "GET", path = "/"] = pos;
  if (!name) die("usage: hibana invoke <app> [METHOD] [PATH] [--body <str>]");
  const cfg = config();
  const token = await login(cfg);
  const headers = {};
  for (const h of [].concat(flags.header || [])) {
    if (typeof h === "string") {
      const idx = h.indexOf(":");
      if (idx > 0) headers[h.slice(0, idx).trim()] = h.slice(idx + 1).trim();
    }
  }
  const envelope = { method, path };
  if (Object.keys(headers).length) envelope.headers = headers;
  if (flags.body && flags.body !== true) envelope.body = flags.body;

  let r = await api(cfg, "POST", "/invoke?wait=1", {
    token,
    json: { component: name, input: envelope },
  });
  // 同期 invoke はサーバ側 SYNC_REPLY_TIMEOUT_MS（既定 5s）を超えると 202 pending に縮退する。
  // 初回はコンポーネントの precompile（JS は十数 MiB）で 5s を超えがちなので、pending の間は
  // GET /executions/{id} を最大 60s ポーリングして終端を待つ。
  if (nonTerminal(r.status) && r.execution_id) {
    const started = Date.now();
    while (nonTerminal(r.status) && Date.now() - started < 60_000) {
      await sleep(600);
      r = await api(cfg, "GET", `/executions/${r.execution_id}`, { token });
    }
  }
  if (r.status !== "succeeded") {
    console.error(red(`status=${r.status}`));
    if (r.error) console.error(JSON.stringify(r.error, null, 2));
    process.exit(1);
  }
  const out = r.output || {};
  // Response エンベロープを HTTP 風に表示
  console.log(bold(`HTTP ${out.status ?? "?"}`));
  for (const [k, v] of Object.entries(out.headers || {})) console.log(`${k}: ${v}`);
  console.log("");
  console.log(out.bodyBase64 ? `(base64) ${out.body}` : out.body ?? "");
}

async function cmdDev(args) {
  const { flags } = parseFlags(args);
  const entry = flags.entry || "src/index.ts";
  const port = Number(flags.port || 8787);
  let app = await loadApp(entry);

  // 本番の native component は `--disable http` で **outbound egress を一切持たない**。
  // dev（Node）は既定で fetch できてしまい「dev では外部 API を叩けるが本番で落ちる」ズレを
  // 生む。忠実なプレビューにするため dev でも outbound fetch を塞ぐ（app が Response/Request を
  // 構築するのは別 API なので影響しない）。--allow-egress で明示的に緩められる。
  if (!flags["allow-egress"]) {
    globalThis.fetch = async () => {
      throw new Error(
        "outbound fetch() is disabled on Hibana (native components have no egress). " +
          "Run `hibana dev --allow-egress` to bypass locally.",
      );
    };
  }

  const server = createServer(async (req, res) => {
    try {
      const url = `http://${req.headers.host || "localhost"}${req.url}`;
      const chunks = [];
      for await (const c of req) chunks.push(c);
      const body =
        chunks.length && req.method !== "GET" && req.method !== "HEAD"
          ? Buffer.concat(chunks)
          : undefined;
      const request = new Request(url, {
        method: req.method,
        headers: req.headers,
        body,
      });
      const r = await app.fetch(request);
      res.statusCode = r.status;
      r.headers.forEach((v, k) => res.setHeader(k, v));
      const buf = Buffer.from(await r.arrayBuffer());
      res.end(buf);
      console.log(dim(`${req.method} ${req.url} → ${r.status}`));
    } catch (e) {
      res.statusCode = 500;
      res.end(String(e && e.stack ? e.stack : e));
      console.error(red(`${req.method} ${req.url} → 500  ${e}`));
    }
  });
  server.listen(port, () => {
    ok(`hibana dev on ${bold(`http://localhost:${port}`)}  ${dim(`(${entry})`)}`);
    console.log(
      dim(
        "  Faithful preview: Web-standard + Hono only (no Node built-ins), no outbound fetch.\n" +
          "  Runs in Node for fast iteration; deploy runs the same app as a wasi:http component.\n" +
          "  Ctrl-C to stop.",
      ),
    );
  });
}

function readStdin() {
  return new Promise((res) => {
    let d = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (c) => (d += c));
    process.stdin.on("end", () => res(d.replace(/\r?\n$/, "")));
  });
}

// Workers 互換: `secret put <app> <NAME>`（値は stdin）/ `secret set <app> <NAME> <VALUE>`。
async function cmdSecret(args) {
  const { pos } = parseFlags(args);
  const [sub, name, key, ...rest] = pos;
  if ((sub !== "set" && sub !== "put") || !name || !key)
    die(
      "usage: hibana secret put <app> <NAME>   (value from stdin)\n" +
        "       hibana secret set <app> <NAME> <VALUE>",
    );
  let value;
  if (sub === "put") {
    value = process.stdin.isTTY ? "" : await readStdin();
    if (!value)
      die("empty value — pipe it:  echo -n <value> | hibana secret put <app> <NAME>");
  } else {
    value = rest.join(" ");
    if (!value) die("secret value is empty");
  }
  const cfg = config();
  const token = await login(cfg);
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  const r = await api(cfg, "PUT", `/components/${id}/secrets/${encodeURIComponent(key)}`, {
    token,
    json: { value },
  });
  ok(`secret ${bold(key)} set on ${name} (version ${r.version})`);
  // Workers 互換: すぐ使えるよう active version へ自動承認（best-effort）。
  const v = await resolveActiveVersion(cfg, token, id);
  if (v) {
    await grantEnvMerged(cfg, token, id, v, [key]);
    console.log(dim(`  granted on ${name}@${v} → read via c.env.${key}`));
  } else {
    console.log(dim(`  approve it:  hibana grant-env ${name} <version> ${key}`));
  }
}

async function cmdConfig(args) {
  const { pos } = parseFlags(args);
  const [sub, name, ...pairs] = pos;
  if (sub !== "set" || !name || pairs.length === 0)
    die("usage: hibana config set <app> KEY=VALUE [KEY=VALUE ...]");
  const cfg = config();
  const token = await login(cfg);
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  // 既存 config を GET してマージ（PUT は全置換なので消さない）。
  let env = {};
  try {
    const cur = await api(cfg, "GET", `/components/${id}/config`, { token });
    if (cur && cur.env) env = { ...cur.env };
  } catch {}
  const setNames = [];
  for (const p of pairs) {
    const i = p.indexOf("=");
    if (i <= 0) die(`bad pair (want KEY=VALUE): ${p}`);
    env[p.slice(0, i)] = p.slice(i + 1);
    setNames.push(p.slice(0, i));
  }
  await api(cfg, "PUT", `/components/${id}/config`, { token, json: { env } });
  ok(`config set on ${name}: ${bold(setNames.join(", "))}`);
  const v = await resolveActiveVersion(cfg, token, id);
  if (v) {
    await grantEnvMerged(cfg, token, id, v, setNames);
    console.log(dim(`  granted on ${name}@${v} → read via c.env`));
  } else {
    console.log(dim(`  approve:  hibana grant-env ${name} <version> ${setNames.join(" ")}`));
  }
}

// admin: この version が受け取れる env 名を承認する（M7b/M9: 値の設定と名前の承認を分離）。
// 既定は **追加**（GET でマージ）。--replace で全置換。
async function cmdGrantEnv(args) {
  const { flags, pos } = parseFlags(args);
  const [name, version, ...names] = pos;
  if (!name || !version || names.length === 0)
    die("usage: hibana grant-env <app> <version> NAME [NAME ...] [--replace]");
  const cfg = config();
  const token = await login(cfg);
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  const merged = await grantEnvMerged(cfg, token, id, version, names, !!flags.replace);
  ok(
    `granted env on ${name}@${version}: ${bold(merged.join(", "))}` +
      (flags.replace ? dim(" (replaced)") : ""),
  );
  console.log(dim(`  Read them in Hono via c.env.NAME (or process.env.NAME).`));
}

async function cmdRollback(args) {
  const { pos } = parseFlags(args);
  const [name, version] = pos;
  if (!name) die("usage: hibana rollback <app> [version]");
  const cfg = config();
  const token = await login(cfg);
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  const r = await api(cfg, "POST", `/components/${id}/rollback`, {
    token,
    json: version ? { version } : {},
  });
  ok(
    `rolled back ${name} → active ${bold(r.active_version_id)}` +
      (r.rolled_back_from ? dim(`  (from ${r.rolled_back_from})`) : ""),
  );
}

async function cmdLogs(args) {
  const { pos } = parseFlags(args);
  const [execId] = pos;
  if (!execId)
    die("usage: hibana logs <execution_id>   (platform exposes GET /executions/{id})");
  const cfg = config();
  const token = await login(cfg);
  const r = await api(cfg, "GET", `/executions/${execId}`, { token });
  console.log(JSON.stringify(r, null, 2));
}

function usage() {
  console.log(
    `${bold("hibana")} — write a Hono app, ship it as a WebAssembly Component\n\n` +
      `  Reads wrangler.toml / wrangler.jsonc (name, main, [vars]) when flags are omitted.\n\n` +
      `  hibana deploy    [--entry src/index.ts] [--name <app>] [--version 0.1.0] [--public]\n` +
      `  hibana dev       [--entry src/index.ts] [--port 8787] [--allow-egress]\n` +
      `  hibana invoke    <app> [METHOD] [PATH] [--body '<str>'] [--header k:v]\n` +
      `  hibana publish   <app>   |   hibana unpublish <app>\n` +
      `  hibana secret    put <app> <NAME>   (stdin)  |  set <app> <NAME> <VALUE>\n` +
      `  hibana config    set <app> KEY=VALUE ...\n` +
      `  hibana grant-env <app> <version> NAME ... [--replace]   approve names → c.env\n` +
      `  hibana rollback  <app> [version]\n` +
      `  hibana logs|tail <execution_id>\n`,
  );
}

async function main() {
  const [cmd, ...rest] = process.argv.slice(2);
  try {
    switch (cmd) {
      case "deploy": return await cmdDeploy(rest);
      case "dev": return await cmdDev(rest);
      case "invoke": return await cmdInvoke(rest);
      case "publish": return await cmdPublish(rest, true);
      case "unpublish": return await cmdPublish(rest, false);
      case "config": return await cmdConfig(rest);
      case "grant-env": return await cmdGrantEnv(rest);
      case "secret": return await cmdSecret(rest);
      case "rollback": return await cmdRollback(rest);
      case "logs":
      case "tail": return await cmdLogs(rest); // `tail` = wrangler 互換エイリアス
      case undefined:
      case "help":
      case "--help":
      case "-h": return usage();
      default:
        die(`unknown command: ${cmd}\n  run \`hibana help\``);
    }
  } catch (e) {
    die(e.message || String(e));
  }
}

main();
