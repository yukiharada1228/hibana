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

## How it works

Your Hono app uses the Web-standard `Request`/`Response`, which
[StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey) backs with
`wasi:http/types`. The build bundles your app + an adapter with esbuild, then
`jco componentize` produces a Component implementing the platform world:

```wit
handle: func(input: list<u8>) -> result<list<u8>, handler-error>
```

The adapter bridges that sync bytes-in/bytes-out contract to
`app.fetch(Request) -> Response` via an HTTP-envelope JSON (`method`/`path`/
`headers`/`body`; binary bodies are base64). `--disable http` drops
`wasi:http/outgoing-handler`, so a deployed component **cannot** make outbound
requests through `wasi:http` — egress stays behind the platform's M9c allowlist.

## Commands

| Command | Description |
|---|---|
| `hibana deploy [--entry src/index.ts] [--name <app>] [--version 0.1.0]` | Build → upload → activate |
| `hibana dev [--entry src/index.ts] [--port 8787]` | Run the Hono app natively on localhost for fast iteration |
| `hibana invoke <app> [METHOD] [PATH] [--body '<str>'] [--header k:v]` | Call a deployed app; renders the HTTP response |
| `hibana secret set <app> <NAME> <VALUE>` | Set a per-function secret (M7c) |
| `hibana rollback <app> [version]` | One-click rollback to the previous stable version (M7) |
| `hibana logs <execution_id>` | Fetch a single execution record |

## Configuration

Connection info comes from the environment (defaults target a local dev stack):

| Var | Default | Purpose |
|---|---|---|
| `HIBANA_URL` | `http://localhost:8080` | Control-plane base URL |
| `HIBANA_TOKEN` | — | Bearer token (skips login if set) |
| `HIBANA_TENANT` | `smoke` | Tenant slug for login |
| `HIBANA_EMAIL` | `admin@example.com` | Login email |
| `HIBANA_PASSWORD` | `dev-password` | Login password |

## Notes

- The first invoke of a freshly deployed component precompiles a ~13 MiB module,
  which can exceed the control-plane's 5 s sync-reply window; `hibana invoke`
  transparently polls `GET /executions/{id}` until the run is terminal.
- `hibana dev` runs your app in Node (not Wasm) so iteration is instant; deploy
  runs it as a Component. Keep app code to Web-standard + Hono APIs so both paths
  behave the same.
