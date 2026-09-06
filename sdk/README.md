# Hibana SDK 🔥

Write a [Hono](https://hono.dev) app, ship it as a WebAssembly Component, run it on your own Hibana infrastructure — the self-hosted Cloudflare Workers experience.

```ts
// src/index.ts
import { Hono } from "hono";
const app = new Hono();
app.get("/", (c) => c.text("Hello from Hono on Hibana 🔥"));
export default app;
```

```console
$ hibana deploy --entry src/index.ts --name my-api

Deploying my-api → http://localhost:8080

  • Building TypeScript → WebAssembly Component ... done
  • Authenticating ... done
  • Uploading version 0.1.0 ... done
  ✓ Deployed my-api@0.1.0

$ hibana invoke my-api GET /
HTTP 200
content-type: text/plain;charset=UTF-8

Hello from Hono on Hibana 🔥
```

## Cloudflare Workers compatibility

Hibana aims to feel like Wrangler. A `wrangler.toml` (or `wrangler.jsonc`) drives
`hibana deploy` with no flags:

```toml
name = "my-worker"
main = "src/index.ts"
compatibility_date = "2024-01-01"   # accepted, no-op (single runtime)

[vars]
GREETING = "Hello"

[hibana]                            # hibana-specific (not in wrangler.toml)
public = true
egress = ["api.example.com:443"]
```

```console
$ hibana deploy        # reads wrangler.toml: build → upload → vars → egress → publish
```

Both worker shapes work — Hono `export default app` **and** plain
`export default { fetch(request, env, ctx) }`. Bindings arrive as `env` /
`c.env` (and `process.env`); `ctx.waitUntil` is a no-op stub.

| Wrangler | Hibana | Notes |
|---|---|---|
| `wrangler deploy` | `hibana deploy` | reads `wrangler.toml`; auto-versions each deploy |
| `wrangler dev` | `hibana dev` | faithful preview (see below) |
| `wrangler secret put NAME` | `hibana secret put <app> NAME` | value from stdin; auto-approved |
| `wrangler tail` | `hibana tail <execution_id>` | single execution (no live stream yet) |
| `wrangler rollback` | `hibana rollback <app>` | one-click to previous version |
| `[vars]` | `[vars]` | applied + approved on deploy |
| `env.MY_VAR` / secrets | `c.env.MY_VAR` / `env.MY_VAR` | ✅ |
| `[[kv_namespaces]]` → `env.KV.get/put/delete/list` | ✅ | Postgres-backed, tenant-scoped |
| `[[r2_buckets]]` → `env.BUCKET.get/put/head/delete/list` | ✅ | MinIO/S3-backed (worker stays keyless) |
| `[[d1_databases]]` → `env.DB.prepare/bind/all/first/run/batch/exec` | ✅ | real SQLite, per-tenant, single-writer |
| `[[queues.producers]]` / `[[queues.consumers]]` → `env.Q.send` + `queue()` | ✅ | on the JetStream pipeline (retries/DLQ) |
| `[[durable_objects.bindings]]` → `env.DO.idFromName/get().fetch()` | ✅ | durable `ctx.storage` + global single-writer (see caveats) |
| `ctx.storage.setAlarm/getAlarm/deleteAlarm` + `alarm()` | ✅ | one-shot DO alarms (re-`setAlarm` in `alarm()` to repeat) |
| `[triggers] crons = [...]` → `scheduled(event, env, ctx)` | ✅ | Cron Triggers on the existing cron scheduler |

### Durable Objects

```toml
[[durable_objects.bindings]]
name = "COUNTER"
class_name = "Counter"
```

```ts
import { DurableObject } from "cloudflare:workers";
export class Counter extends DurableObject {
  async fetch(req: Request) {
    let n = (await this.ctx.storage.get<number>("n")) ?? 0;
    n++; await this.ctx.storage.put("n", n);
    return new Response(String(n));
  }
}
// in your Hono app:
const stub = c.env.COUNTER.get(c.env.COUNTER.idFromName("room-1"));
return stub.fetch(req);
```

`idFromName` / `newUniqueId` / `get(id).fetch()` and `ctx.storage`
(`get`/`put`/`delete`/`list`) are supported. Each object id is a **global
single-writer**: a Postgres advisory lock on `(tenant, class, id)` serializes all
access across the fleet, and `ctx.storage` is durable (per-tenant). `hibana dev`
runs objects in-process with in-memory storage.

**Alarms** are supported via `ctx.storage.setAlarm(when)` /
`getAlarm()` / `deleteAlarm()` and an `alarm()` method on the class:

```ts
export class Reminder extends DurableObject {
  async fetch() { await this.ctx.storage.setAlarm(Date.now() + 60_000); return new Response("armed"); }
  async alarm() {
    // fired ~60s later, under the object's single-writer lock
    await this.ctx.storage.put("ranAt", Date.now());
    // await this.ctx.storage.setAlarm(Date.now() + 60_000); // re-arm for a periodic timer
  }
}
```

Alarms are **one-shot** (re-`setAlarm` inside `alarm()` to repeat) and fire on the
same enqueue pipeline as cron/queues (provenance, metering, retries).

> **Subset.** This is a per-invocation activation model: durable storage +
> serialization (the core coordination guarantee) + alarms, suitable for counters,
> locks, per-entity state, and timers. In-memory state does **not** persist between
> calls, and WebSocket hibernation is not yet implemented — that needs a resident
> placement runtime (a future milestone).

### Queues

```toml
[[queues.producers]]
binding = "MY_QUEUE"
queue = "tasks"
[[queues.consumers]]
queue = "tasks"
```

```ts
const app = new Hono();
app.post("/enqueue", async (c) => { await c.env.MY_QUEUE.send({ hello: "world" }); return c.text("queued"); });
export default {
  fetch: (req, env, ctx) => app.fetch(req, env, ctx),
  async queue(batch, env) {
    for (const m of batch.messages) console.log(m.body); // process; throw to retry the whole batch
  },
};
```

`send` / `sendBatch` enqueue messages as **consumer invocations on the same
JetStream pipeline** that powers HTTP/cron — so retries, backoff, and the
dead-letter path come for free. A consumer that throws fails the execution and is
retried (then dead-lettered after `MAX_DELIVER`). `hibana dev` loops producers
straight into your `queue()` handler in-process.

### Cron Triggers

```toml
[triggers]
crons = ["*/5 * * * *", "0 0 * * *"]
```

```ts
export default {
  fetch: (req, env, ctx) => app.fetch(req, env, ctx),
  async scheduled(event, env, ctx) {
    // event.cron = the matched pattern, event.scheduledTime = fire time (ms)
    console.log("tick", event.cron);
  },
};
```

`hibana deploy` registers each `[triggers].crons` entry with the cron scheduler
and re-syncs on every deploy (stale hibana-managed triggers are replaced, your
manually-created cron jobs are left alone). Fires dispatch to `scheduled()` on the
same enqueue pipeline as everything else. Schedules use standard 5-field cron
(minute granularity).

### D1

```toml
[[d1_databases]]
binding = "DB"
database_name = "app-db"
```

```ts
await c.env.DB.exec("CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, name TEXT)");
await c.env.DB.prepare("INSERT INTO users(name) VALUES (?)").bind("alice").run();
const { results } = await c.env.DB.prepare("SELECT * FROM users").all();
const one = await c.env.DB.prepare("SELECT * FROM users WHERE id=?").bind(1).first();
await c.env.DB.batch([stmt1, stmt2]); // atomic
```

Each database is a **real, isolated SQLite** persisted per `(tenant, name)`. A
Postgres **session advisory lock** makes access single-writer — concurrent
executions touching the same DB serialize — and the DB is loaded once per request
and saved at the end. Guest SQL is sandboxed with a SQLite authorizer that denies
`ATTACH`/extension loading, so it can never reach the filesystem or the platform's
own database. `hibana dev` uses Node's `node:sqlite` (in-memory).

> This request-scoped resident model is the stateful foundation; the same
> advisory-lock + persist pattern is what Durable Objects/Queues will build on.

### KV

```toml
[[kv_namespaces]]
binding = "CACHE"
id = "my-cache"   # namespace (partition within your tenant)
```

```ts
app.get("/cache/:k", async (c) => (await c.env.CACHE.get(c.req.param("k"))) ?? c.notFound());
app.put("/cache/:k", async (c) => { await c.env.CACHE.put(c.req.param("k"), await c.req.text(), { expirationTtl: 3600 }); return c.text("ok"); });
```

`get` / `put` (with `expirationTtl`) / `delete` / `list({ prefix, limit })` are
supported. Values persist in the platform's Postgres, isolated **per tenant** by
row-level security — a component can never read another tenant's data. Namespaces
partition data within a tenant (they are not a security boundary between your own
components). `hibana dev` uses a non-persistent in-memory KV.

### R2

```toml
[[r2_buckets]]
binding = "MEDIA"
bucket_name = "media"
```

```ts
await c.env.MEDIA.put(key, value, {
  httpMetadata: { contentType: "image/png" },
  customMetadata: { owner: "yuki" },
});
const obj = await c.env.MEDIA.get(key); // .text() / .json() / .arrayBuffer(), .size, .etag, .httpMetadata, .customMetadata
```

`put` / `get` / `head` / `delete` / `list({ prefix, limit })` are supported, with
`httpMetadata.contentType` and `customMetadata`. Objects are stored in the
platform's **MinIO/S3** under `r2/{tenant}/{bucket}/{key}`. The worker stays
**keyless** — R2 I/O is proxied through a control-plane internal endpoint that
authenticates the job token and does the S3 op for the caller's tenant, so a
component can only reach its own tenant's objects. `hibana dev` uses in-memory R2.

**`get` (download) streams** end-to-end (S3 → control-plane → worker → your
handler) without buffering the object in memory, so large objects are cheap to
serve. **`put` (upload)** streams the body through the worker to the control-plane
without buffering it on the (multi-tenant) worker; the control-plane still buffers
before the S3 write, so objects are capped at **100 MiB**. Fully streamed uploads
(S3 multipart, no control-plane buffering) are a planned follow-up.

Secrets set with `hibana secret put` persist across redeploys (each new version
inherits the previous version's approved names).

## How it works

Your Hono app is compiled to a **native `wasi:http` component** — it exports
`wasi:http/incoming-handler`, the standard WASI HTTP world. The build bundles your
app with a one-line `fetch`-event shim and Hono via esbuild, then `jco componentize`
produces the component:

```wit
world http {
  export wasi:http/incoming-handler@0.2.3;
}
```

[StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey) wires the
`fetch` event straight to `incoming-handler`, so the platform's worker hands your
app a real `Request` and gets a real `Response` back — **no adapter, no JSON
envelope, no base64** at the app layer.

**Outbound `fetch` (egress)** is deny-by-default and gated by an admin-approved
allowlist (the M9c model). A deployed app can only reach hosts explicitly approved
for its version; everything else — and any private/loopback/metadata IP (SSRF
hard-deny) — is refused. Approve hosts with:

```
PUT /components/{id}/versions/{version}/capabilities/egress
    {"allow_outbound": ["api.example.com:443"]}
```

With no approved hosts, `fetch()` fails (matching `hibana dev`, which blocks it
unless you pass `--allow-egress`).

> The worker runs both worlds: native `wasi:http` components (Hono, and any
> language's HTTP framework) and the older bytes-in/bytes-out `handle` world used
> by queue/cron/event/chain functions.

## Commands

| Command | Description |
|---|---|
| `hibana deploy [--entry src/index.ts] [--name <app>] [--version 0.1.0] [--public]` | Build → upload → activate → warm up (→ publish, with `--public`) |
| `hibana dev [--entry src/index.ts] [--port 8787] [--allow-egress]` | Run the app on localhost — a faithful preview of the Wasm runtime |
| `hibana invoke <app> [METHOD] [PATH] [--body '<str>'] [--header k:v]` | Call a deployed app; renders the HTTP response |
| `hibana publish <app>` / `hibana unpublish <app>` | Turn the public URL on/off (deny-by-default) |
| `hibana secret set <app> <NAME> <VALUE>` | Set a per-function secret (M7c) |
| `hibana config set <app> KEY=VALUE ...` | Set plaintext config values (merged) |
| `hibana grant-env <app> <version> NAME ...` | Approve which env names the app may read (admin; all-replace) |
| `hibana rollback <app> [version]` | One-click rollback to the previous stable version (M7) |
| `hibana logs <execution_id>` | Fetch a single execution record |

## Public URLs

A deployed app is reachable at a real HTTP URL once you publish it:

```console
$ HIBANA_INGRESS_DOMAIN=hibana.local hibana deploy --entry src/index.ts --name my-api --public
  ...
  ✓ Deployed my-api@0.1.0
  → http://my-api.smoke.hibana.local/
```

The platform's ingress gateway resolves `<app>.<tenant>.<base>` from the `Host`
header, turns the request into an invoke, and returns the app's real HTTP
response — no auth token needed (it's a public endpoint, like Cloudflare Workers).

- **Opt-in, deny-by-default**: only apps you explicitly `publish` (or deploy with
  `--public`) are reachable; everything else — and every unknown app/tenant/host —
  returns `404`. This is *ingress* only; function *egress* stays behind the M9c
  allowlist regardless.
- **Server config**: the control-plane must run with `INGRESS_BASE_DOMAIN` set
  (e.g. `hibana.local`) for the gateway to be active; unset disables it entirely.
  Set `HIBANA_INGRESS_DOMAIN` to the same value so the CLI prints the URL.
- **Local dev** (no wildcard DNS): send the `Host` header directly —
  `curl -H "Host: my-api.smoke.hibana.local" http://localhost:8080/`.

## Config & secrets

Your app reads config and secrets as **`c.env`** (Workers-style, the 2nd arg to
`fetch`) or **`process.env`**:

```ts
const app = new Hono<{ Bindings: { API_KEY: string } }>();
app.get("/", (c) => fetch("https://api.example.com", {
  headers: { authorization: `Bearer ${c.env.API_KEY}` },
}));
```

Two steps (the platform separates *setting a value* from *approving the name*):

```console
$ hibana secret set my-api API_KEY sk-...      # or: hibana config set my-api FOO=bar
$ hibana grant-env my-api 0.1.0 API_KEY FOO    # admin approves injectable names (all-replace)
```

Only approved names are injected. Under the hood the worker passes them to your
component out-of-band and the SDK exposes them as `c.env` / `process.env` — they
are **not** visible as request headers to your handler.

## `hibana dev` is a faithful preview

`hibana dev` runs your app in Node for instant iteration, but constrains it to
match the Wasm runtime so you catch divergences early:

- **Same bundling as deploy** (`platform: neutral`): importing a Node built-in
  (`node:fs`, etc.) fails in `dev` exactly as it would fail the production build.
- **No outbound `fetch`**: native components have no egress, so `dev` blocks
  `fetch()` too. Pass `--allow-egress` to bypass locally when you knowingly need
  it during development.

Keep app code to Web-standard + Hono APIs and `dev` behaves like production.

## Configuration

Connection info comes from the environment (defaults target a local dev stack):

| Var | Default | Purpose |
|---|---|---|
| `HIBANA_URL` | `http://localhost:8080` | Control-plane base URL |
| `HIBANA_TOKEN` | — | Bearer token (skips login if set) |
| `HIBANA_TENANT` | `smoke` | Tenant slug for login |
| `HIBANA_EMAIL` | `admin@example.com` | Login email |
| `HIBANA_PASSWORD` | `dev-password` | Login password |
| `HIBANA_INGRESS_DOMAIN` | — | Public ingress base domain; set to print URLs (match the server's `INGRESS_BASE_DOMAIN`) |

## Notes

- The first invoke of a freshly deployed component precompiles a ~13 MiB module.
  `hibana deploy` warms it once so the returned URL is fast; `hibana invoke` polls
  `GET /executions/{id}` until the run is terminal (handling `running`, not just
  `pending`). The worker sends in-progress acks during precompile and dedups
  concurrent compiles of the same module (no redelivery storm / stampede).
