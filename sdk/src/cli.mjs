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
async function loadApp(entry) {
  const { build } = await import("esbuild");
  const work = await mkdtemp(join(tmpdir(), "hibana-dev-"));
  const outfile = join(work, "app.mjs");
  await build({
    entryPoints: [resolve(process.cwd(), entry)],
    bundle: true,
    format: "esm",
    platform: "node",
    target: "es2022",
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
  const entry = flags.entry || "src/index.ts";
  const name = flags.name || "app";
  const version = flags.version || "0.1.0";
  const cfg = config();

  console.log(bold(`\nDeploying ${name} → ${cfg.url}\n`));

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

  let published = false;
  if (flags.public) {
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
    console.log(dim("  Ctrl-C to stop. Runs your Hono app natively for fast iteration."));
  });
}

async function cmdSecret(args) {
  const [sub, name, key, ...rest] = args;
  if (sub !== "set" || !name || !key)
    die("usage: hibana secret set <app> <NAME> <VALUE>");
  const value = rest.join(" ");
  if (!value) die("secret value is empty");
  const cfg = config();
  const token = await login(cfg);
  const id = await resolveComponentId(cfg, token, name);
  if (!id) die(`no such component: ${name}`);
  const r = await api(cfg, "PUT", `/components/${id}/secrets/${encodeURIComponent(key)}`, {
    token,
    json: { value },
  });
  ok(`secret ${bold(key)} set on ${name} (version ${r.version})`);
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
      `  hibana deploy    [--entry src/index.ts] [--name <app>] [--version 0.1.0] [--public]\n` +
      `  hibana dev       [--entry src/index.ts] [--port 8787]\n` +
      `  hibana invoke    <app> [METHOD] [PATH] [--body '<str>'] [--header k:v]\n` +
      `  hibana publish   <app>        make reachable at <app>.<tenant>.<base>\n` +
      `  hibana unpublish <app>        disable public ingress\n` +
      `  hibana secret    set <app> <NAME> <VALUE>\n` +
      `  hibana rollback  <app> [version]\n` +
      `  hibana logs      <execution_id>\n`,
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
      case "secret": return await cmdSecret(rest);
      case "rollback": return await cmdRollback(rest);
      case "logs": return await cmdLogs(rest);
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
