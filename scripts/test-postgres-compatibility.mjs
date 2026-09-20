// Packaged PostgreSQL adapter -> Hono -> Wasm -> disposable PostgreSQL.
// Only the trusted Wasmtime test fixture inherits network access.
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { runCommand } from "./bounded-process.mjs";
import { packExtensions } from "./pack-extensions.mjs";
import { authenticationFixture } from "./postgres-auth-fixture.mjs";
import { writePostgresSelection } from "./postgres-selection.mjs";
import { resolveExtensions } from "../sdk/src/extensions.mjs";

const execute = promisify(execFile);
const root = fileURLToPath(new URL("..", import.meta.url));
const fixture = join(root, "scripts/fixtures/postgres");
const require = createRequire(join(fixture, "package.json"));
const pg = require("pg");
const { build: bundleJavaScript } = createRequire(
  join(root, "sdk/package.json"),
)("esbuild");
const folder = await mkdtemp(join(tmpdir(), "hibana-pg-probe-"));
const reportDir = resolve(
  process.env.HIBANA_PG_REPORT_DIR ||
    join(root, ".local/verification/postgres"),
);
const container = `hibana-pg-probe-${randomUUID()}`;
const report = {
  date: new Date().toISOString(),
  node: process.version,
  pg: require("pg/package.json").version,
  hono: JSON.parse(
    await readFile(join(fixture, "node_modules/hono/package.json")),
  ).version,
  drizzle: JSON.parse(
    await readFile(join(fixture, "node_modules/drizzle-orm/package.json")),
  ).version,
  wasmDatabaseQuery: "not tested",
};
let started = false;
let runtime,
  runtimeLog = "";
const application = join(folder, "application");
const artifact = join(folder, "postgres.wasm");
const tcpArtifact = join(folder, "postgres-tcp.wasm");
const selectedArtifacts = Object.fromEntries(
  ["scram", "md5", "noCertificateHash", "clientOnly", "clientWithPool"].map(
    (name) => [name, join(folder, `selected-${name}.wasm`)],
  ),
);

async function run(command, args, options = {}) {
  const { stdout } = await execute(command, args, {
    timeout: 120000,
    killSignal: "SIGKILL",
    maxBuffer: 2 * 1024 * 1024,
    ...options,
  });
  return stdout.trim();
}

async function buildApplication() {
  await mkdir(application);
  for (const name of [
    "package.json",
    "package-lock.json",
    "app.mjs",
    "client-only.mjs",
    "schema.mjs",
  ])
    await copyFile(join(fixture, name), join(application, name));
  await run(
    "npm",
    ["ci", "--offline", "--ignore-scripts", "--no-audit", "--no-fund"],
    { cwd: application },
  );
  const packed = await packExtensions(folder);
  const tarballs = packed.map((pkg) => pkg.tarball);
  report.extensions = packed.map(({ name, version, integrity }) => ({
    name,
    version,
    integrity,
  }));
  await run(
    "npm",
    [
      "install",
      "--ignore-scripts",
      "--no-audit",
      "--no-fund",
      "--no-save",
      "--package-lock=false",
      ...tarballs,
    ],
    { cwd: application },
  );
  await writeFile(
    join(application, "hibana.json"),
    JSON.stringify(
      {
        name: "postgres-probe",
        main: "app.mjs",
        extensions: ["@hibana/postgres"],
      },
      null,
      2,
    ),
  );
  let output = "";
  try {
    const result = await execute(
      process.execPath,
      [join(root, "sdk/src/cli.mjs"), "build"],
      {
        cwd: application,
        timeout: 180000,
        killSignal: "SIGKILL",
        maxBuffer: 2 * 1024 * 1024,
      },
    );
    output = result.stdout + result.stderr;
    report.hibanaBuild = "passed";
  } catch (error) {
    output = (error.stdout || "") + (error.stderr || "");
    report.hibanaBuild = "failed";
    throw error;
  } finally {
    await writeFile(
      join(reportDir, "hibana-build.log"),
      output.replaceAll(folder, "<temporary>"),
    );
  }
  await copyFile(join(application, ".hibana/build/app.wasm"), artifact);
  await writeFile(
    join(application, "hibana.json"),
    JSON.stringify({
      name: "postgres-probe",
      main: "app.mjs",
      extensions: ["@hibana/postgres-tcp"],
    }),
  );
  await execute(process.execPath, [join(root, "sdk/src/cli.mjs"), "build"], {
    cwd: application,
    timeout: 180000,
    killSignal: "SIGKILL",
    maxBuffer: 2 * 1024 * 1024,
  });
  await copyFile(join(application, ".hibana/build/app.wasm"), tcpArtifact);
  report.artifactBytes = {
    full: (await readFile(artifact)).byteLength,
    tcpOnly: (await readFile(tcpArtifact)).byteLength,
  };
  assert.ok(report.artifactBytes.tcpOnly < report.artifactBytes.full);
  report.selections = {};
  for (const [name, options] of [
    ["scram", {}],
    ["md5", { tls: false, scram: false, md5: true }],
    ["noCertificateHash", { certificateHashes: [] }],
    ["clientOnly", { pool: false }],
    ["clientWithPool", { pool: true }],
  ]) {
    const selection = await writePostgresSelection(application, options);
    const config = {
      name: "postgres-probe",
      main: name.startsWith("client") ? "client-only.mjs" : "app.mjs",
      extensions: [selection],
    };
    await writeFile(join(application, "hibana.json"), JSON.stringify(config));
    const plan = await resolveExtensions({ ...config, root: application });
    await execute(process.execPath, [join(root, "sdk/src/cli.mjs"), "build"], {
      cwd: application,
      timeout: 180000,
      killSignal: "SIGKILL",
      maxBuffer: 2 * 1024 * 1024,
    });
    const bytes = await readFile(join(application, ".hibana/build/app.wasm"));
    await writeFile(selectedArtifacts[name], bytes);
    report.selections[name] = {
      artifactBytes: bytes.length,
      imports: plan.imports,
    };
    if (name.startsWith("client")) {
      const bundle = await bundleJavaScript({
        stdin: {
          contents: [
            ...plan.preload.map((path) => `import ${JSON.stringify(path)};`),
            `export {default} from ${JSON.stringify(join(application, config.main))};`,
          ].join("\n"),
          resolveDir: application,
        },
        absWorkingDir: application,
        bundle: true,
        format: "esm",
        platform: "browser",
        target: "es2022",
        mainFields: ["module", "main"],
        conditions: ["import", "default"],
        alias: plan.aliases,
        external: plan.imports,
        write: false,
        metafile: true,
      });
      assert.equal(
        Object.keys(bundle.metafile.inputs).some((path) =>
          path.endsWith("/postgres-pool/dist/index.mjs"),
        ),
        name === "clientWithPool",
      );
      report.selections[name].javascriptBytes =
        bundle.outputFiles[0].contents.length;
    }
    assert.ok(bytes.length < report.artifactBytes.full);
  }
  // Compare the JS input. The initialized JS engine's Wasm memory snapshot
  // does not grow monotonically with the amount of application source.
  assert.ok(
    report.selections.clientOnly.javascriptBytes <
      report.selections.clientWithPool.javascriptBytes,
    "The same Client application must be smaller without Pool",
  );
  const cp = resolve(
    process.env.HIBANA_TEST_CP_BIN || "target/debug/hibana-control-plane",
  );
  const validation = JSON.parse(
    await runCommand(cp, ["--validate-stdin"], {
      input: await readFile(artifact),
      timeoutMs: 120000,
    }),
  );
  assert.ok(validation.Ok, JSON.stringify(validation));
  assert.ok(
    validation.Ok.approved_imports.every((name) => name.startsWith("wasi:")),
  );
  report.hostImports = validation.Ok.approved_imports;
  console.log(
    "PASS published tarballs, Hono/Drizzle Wasm build and control-plane validation (standard WASI only)",
  );
}

async function stopRuntime() {
  if (!runtime?.pid || runtime.exitCode !== null || runtime.signalCode !== null)
    return;
  const exited = new Promise((done) => runtime.once("exit", done));
  runtime.kill("SIGTERM");
  const timer = setTimeout(() => runtime.kill("SIGKILL"), 2000);
  await exited;
  clearTimeout(timer);
}
async function launch(command, args, pattern) {
  runtimeLog = "";
  runtime = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
  let failure;
  runtime.on("error", (error) => {
    failure = error;
  });
  for (const stream of [runtime.stdout, runtime.stderr])
    stream.on("data", (chunk) => {
      runtimeLog = (runtimeLog + chunk).slice(-65536);
    });
  for (let attempt = 0; attempt < 1200; attempt++) {
    if (failure) throw failure;
    assert.equal(runtime.exitCode, null, runtimeLog);
    assert.equal(runtime.signalCode, null, runtimeLog);
    const match = runtimeLog.match(pattern);
    if (match) return match[1];
    await sleep(100);
  }
  throw new Error("Runtime startup deadline: " + runtimeLog);
}

async function checkWasm(config, ca) {
  const wasmtime = process.env.WASMTIME_BIN || "wasmtime";
  let url = await launch(
    wasmtime,
    [
      "serve",
      "--addr",
      "127.0.0.1:0",
      "-S",
      "cli=y,inherit-network=y,allow-ip-name-lookup=y,tcp=y,udp=n",
      "-W",
      "timeout=30s",
      artifact,
    ],
    /Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/,
  );
  const env = {
    DB_HOST: "localhost",
    DB_PORT: String(config.port),
    DB_NAME: config.database,
    DB_USER: config.user,
    DB_PASSWORD: config.password,
    DB_CA: ca,
  };
  report.requests = [];
  async function call(path = "/", overrides = {}, status = 200) {
    const started = performance.now();
    const sample = { path, status: null, durationMs: null };
    report.requests.push(sample);
    try {
      const response = await fetch(url + path, {
        headers: {
          "x-hibana-env": Buffer.from(
            JSON.stringify({ ...env, ...overrides }),
          ).toString("base64url"),
        },
        signal: AbortSignal.timeout(35000),
      });
      sample.status = response.status;
      const text = await response.text();
      assert.equal(response.status, status, `${path}: ${text}\n${runtimeLog}`);
      return JSON.parse(text);
    } finally {
      sample.durationMs = Math.round(performance.now() - started);
    }
  }
  for (const tls of ["off", "trusted"]) {
    const result = await call("/", { DB_TLS: tls });
    assert.deepEqual(result, {
      answer: 42,
      greeting: "こんにちは Hibana 🔥",
      document: { value: "json" },
      bytes: [0, 1, 128, 255],
      ssl: tls === "trusted",
    });
    console.log(
      `PASS Wasm pg: ${tls === "trusted" ? "TLS" : "TCP"}, SCRAM, parameters, UTF-8, JSONB and bytea`,
    );
  }
  assert.deepEqual(await call("/callback"), [[42]]);
  assert.deepEqual(await call("/prepared"), [1, 2, 3]);
  assert.deepEqual(await call("/sql-error"), { code: "42P01", answer: 42 });
  assert.deepEqual(await call("/pool"), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.deepEqual(await call("/password-provider"), { answer: 42 });
  for (const tls of ["off", "trusted"])
    for (const api of ["promise", "callback"])
      assert.deepEqual(
        await call(`/cancel-connect?api=${api}`, { DB_TLS: tls }),
        {
          code: "ERR_PG_CONNECTION_CANCELLED",
          notifications: 1,
          closed: true,
        },
      );
  report.connectionCancellation =
    "passed (Client.end settles pending Promise/callback connect once over TCP/TLS; repeated end and late password completion are safe)";
  console.log(
    "PASS Wasm pg: end cancels pending authentication over TCP/TLS for Promise and callback APIs",
  );
  for (const tls of ["off", "trusted"])
    for (const api of ["promise", "callback"])
      for (const sqlError of [false, true])
        assert.deepEqual(
          await call(`/pipeline-end?api=${api}&error=${sqlError}`, {
            DB_TLS: tls,
          }),
          {
            results: [
              sqlError ? { code: "22012" } : { value: 1 },
              { value: 42 },
            ],
            notifications: [1, 1],
            endNotifications: 1,
            closed: true,
          },
        );
  report.pipelineShutdown =
    "passed (in-flight parameterized queries and SQL error recovery drain before end over TCP/TLS, Promise/callback APIs, query timeout disabled)";
  console.log(
    "PASS Wasm pg: pipeline end drains queries and SQL errors over TCP/TLS without a query timeout",
  );
  assert.deepEqual(await call("/invalid-ports"), { client: 21, pool: 21 });
  for (const owner of ["client", "pool"])
    for (const api of ["promise", "callback"])
      for (const method of ["connect", "setNoDelay"])
        assert.deepEqual(
          await call(
            `/connect-error?owner=${owner}&api=${api}&method=${method}`,
          ),
          {
            code: "ERR_FIXTURE_CONNECT",
            notifications: 1,
            closed: true,
            remaining: 0,
          },
        );
  report.connectionStartFailure =
    "passed (invalid Client/Pool ports rejected before transport creation; synchronous transport errors settle Promise/callback connect and end, with sockets released)";
  console.log(
    "PASS Wasm pg: invalid ports rejected; synchronous transport errors close Client/Pool without hanging",
  );
  assert.deepEqual(await call("/options"), [
    "password",
    "channelBinding",
    "channelBindingWithoutTls",
    "channelBindingOption",
    "verification",
    "clientCertificate",
    "filesystem",
    "scramLimit",
    "native",
    "poolLifetime",
    "poolRotation",
  ]);
  console.log(
    "PASS Wasm pg: callback, rowMode, prepared statements, SQL error recovery and pooled concurrency",
  );
  for (const mode of ["require", "prefer", "disable"]) {
    assert.deepEqual(
      await call("/channel-binding", {
        DB_TLS: "trusted",
        DB_CHANNEL_BINDING: mode,
      }),
      {
        answer: 42,
        channelBinding: mode !== "disable",
      },
    );
  }
  assert.deepEqual(await call("/channel-binding-url", { DB_TLS: "trusted" }), {
    channelBinding: true,
  });
  assert.deepEqual(
    await call("/channel-binding", {
      DB_TLS: "off",
      DB_CHANNEL_BINDING: "prefer",
    }),
    { answer: 42, channelBinding: false },
  );
  assert.match(
    (
      await call(
        "/channel-binding",
        { DB_TLS: "off", DB_CHANNEL_BINDING: "require" },
        500,
      )
    ).error,
    /needs TLS/,
  );
  const mismatch = await call(
    "/channel-binding",
    {
      DB_TLS: "trusted",
      DB_CHANNEL_BINDING: "require",
      DB_TAMPER_CERT: "true",
    },
    500,
  );
  assert.match(mismatch.error, /channel binding|authentication/i);
  const authFixture = await authenticationFixture({
    cert: ca,
    key: await readFile(join(folder, "server.key")),
  });
  try {
    for (const mode of ["scram", "cleartext", "md5", "trust"]) {
      authFixture.mode = mode;
      const rejected = await call(
        "/channel-binding",
        {
          DB_TLS: "trusted",
          DB_CHANNEL_BINDING: "require",
          DB_PORT: String(authFixture.port),
        },
        500,
      );
      assert.equal(rejected.code, "ERR_PG_CHANNEL_BINDING", mode);
    }
    for (const mode of [
      "early-cleartext",
      "early-md5",
      "duplicate-md5",
      "early-scram-continuation",
      "early-scram-final",
    ]) {
      authFixture.mode = mode;
      const rejected = await call(
        "/channel-binding",
        {
          DB_TLS: "trusted",
          DB_CHANNEL_BINDING: "prefer",
          DB_PORT: String(authFixture.port),
        },
        500,
      );
      assert.equal(rejected.code, "ERR_PG_AUTH_UNAVAILABLE", mode);
    }
    assert.equal(
      authFixture.credentialOrQueryMessages,
      0,
      "No password, proof or query may follow a rejected authentication method",
    );
  } finally {
    await authFixture.close();
  }
  report.channelBinding =
    "passed (SCRAM-SHA-256-PLUS, URL policy, required/preferred/disabled, tampered certificate and authentication downgrade refusal)";
  report.authenticationOrder =
    "passed (early success, duplicate password challenge, premature SCRAM continuation/final rejected before sending credentials or queries)";
  console.log(
    "PASS Wasm authentication rejects out-of-order messages without sending credentials or queries",
  );
  console.log(
    "PASS Wasm channel binding: verified certificate, server proof, URL require policy; plaintext, mismatched binding, unbound SCRAM, password/MD5 and trust downgrade refused",
  );
  for (const tls of ["off", "trusted"])
    assert.deepEqual(await call("/orm", { DB_TLS: tls }), {
      result: { id: 1, label: "更新 🔥" },
      remaining: [],
    });
  console.log(
    "PASS Wasm Drizzle: insert/update/select/delete, commit and rollback over TCP and verified TLS",
  );
  assert.equal((await call("/", { DB_PASSWORD: "wrong" }, 500)).code, "28P01");
  for (const tls of ["untrusted", "wronghost"]) {
    const rejected = await call(
      "/",
      { DB_TLS: tls, DB_HOST: "127.0.0.1" },
      500,
    );
    assert.match(
      rejected.code || "",
      /TLS|CERT/,
      JSON.stringify({ tls, rejected }),
    );
  }
  assert.equal(
    (await call("/", { DB_TLS: "insecure" }, 500)).code,
    "ERR_NOT_SUPPORTED",
  );
  assert.match((await call("/timeout", {}, 500)).error, /timeout/i);
  assert.deepEqual(await call("/repeat"), { connections: 40 });
  report.wasmDatabaseQuery =
    "passed (TCP/TLS, SCRAM, parameters, types, callbacks, prepared statements, pool, errors, timeout, 40 sequential connections)";
  report.wasmOrm =
    "passed (Drizzle 0.45.2 CRUD, commit, rollback over TCP/TLS)";
  console.log(
    "PASS Wasm pg: wrong password, invalid certificates, verification bypass and timeout rejected; repeated connections released",
  );
  await stopRuntime();
  url = await launch(
    wasmtime,
    [
      "serve",
      "--addr",
      "127.0.0.1:0",
      "-S",
      "cli=y,inherit-network=y,allow-ip-name-lookup=y,tcp=y,udp=n",
      "-W",
      "timeout=30s",
      tcpArtifact,
    ],
    /Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/,
  );
  const plain = await call("/", { DB_TLS: "off" });
  assert.equal(plain.answer, 42);
  assert.equal(plain.ssl, false);
  assert.deepEqual(await call("/pool"), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.deepEqual(await call("/orm"), {
    result: { id: 1, label: "更新 🔥" },
    remaining: [],
  });
  assert.equal(
    (await call("/", { DB_TLS: "trusted" }, 500)).code,
    "ERR_PG_TLS_UNAVAILABLE",
  );
  assert.equal(
    (await call("/tls-required-url", {}, 500)).code,
    "ERR_PG_TLS_UNAVAILABLE",
  );
  report.tcpOnly =
    "passed (separate artifact, SCRAM, queries, pool, Drizzle; TLS options and sslmode=require rejected before connecting)";
  console.log(
    "PASS PostgreSQL TCP-only artifact: queries/Pool/Drizzle work; TLS options and required-TLS URLs fail closed",
  );
  await stopRuntime();
  for (const [name, selected] of Object.entries(selectedArtifacts)) {
    url = await launch(
      wasmtime,
      [
        "serve",
        "--addr",
        "127.0.0.1:0",
        "-S",
        "cli=y,inherit-network=y,allow-ip-name-lookup=y,tcp=y,udp=n",
        "-W",
        "timeout=30s",
        selected,
      ],
      /Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/,
    );
    if (name.startsWith("client")) {
      for (const secure of [false, true])
        assert.deepEqual(
          await call("/", { DB_TLS: secure ? "trusted" : "off" }),
          {
            answer: 42,
            greeting: "こんにちは 🔥",
            hasPool: name === "clientWithPool",
            channelBinding: secure,
          },
        );
    } else if (name === "scram") {
      assert.deepEqual(
        await call("/channel-binding", {
          DB_TLS: "trusted",
          DB_CHANNEL_BINDING: "require",
        }),
        { answer: 42, channelBinding: true },
      );
      assert.deepEqual(
        await call("/pool", { DB_TLS: "trusted" }),
        [0, 1, 2, 3, 4, 5, 6, 7],
      );
      assert.deepEqual(await call("/orm", { DB_TLS: "trusted" }), {
        result: { id: 1, label: "更新 🔥" },
        remaining: [],
      });
      const peer = await authenticationFixture({
        cert: ca,
        key: await readFile(join(folder, "server.key")),
      });
      try {
        for (const mode of ["cleartext", "md5", "trust"]) {
          peer.mode = mode;
          assert.equal(
            (
              await call(
                "/channel-binding",
                {
                  DB_TLS: "trusted",
                  DB_CHANNEL_BINDING: "disable",
                  DB_PORT: String(peer.port),
                },
                500,
              )
            ).code,
            "ERR_PG_AUTH_UNAVAILABLE",
          );
        }
        assert.equal(
          peer.credentialOrQueryMessages,
          0,
          "Unselected authentication must not send credentials or queries",
        );
      } finally {
        await peer.close();
      }
    } else if (name === "md5") {
      assert.equal(
        (await call("/", { DB_USER: "probe_md5", DB_TLS: "off" })).answer,
        42,
      );
      assert.deepEqual(
        await call("/pool", { DB_USER: "probe_md5", DB_TLS: "off" }),
        [0, 1, 2, 3, 4, 5, 6, 7],
      );
      assert.equal(
        (await call("/", { DB_TLS: "off" }, 500)).code,
        "ERR_PG_AUTH_UNAVAILABLE",
      );
      assert.equal(
        (await call("/", { DB_TLS: "trusted" }, 500)).code,
        "ERR_PG_TLS_UNAVAILABLE",
      );
    } else {
      assert.equal(
        (
          await call(
            "/channel-binding",
            { DB_TLS: "trusted", DB_CHANNEL_BINDING: "require" },
            500,
          )
        ).code,
        "ERR_PG_CERTIFICATE_DIGEST_UNAVAILABLE",
      );
      assert.equal(
        (
          await call(
            "/channel-binding",
            { DB_TLS: "trusted", DB_CHANNEL_BINDING: "prefer" },
            500,
          )
        ).code,
        "ERR_PG_CERTIFICATE_DIGEST_UNAVAILABLE",
      );
      assert.deepEqual(
        await call("/channel-binding", {
          DB_TLS: "trusted",
          DB_CHANNEL_BINDING: "disable",
        }),
        { answer: 42, channelBinding: false },
      );
    }
    report.selections[name].verification = "passed";
    console.log(
      `PASS selected PostgreSQL ${name}: actual Wasm connection and omitted-feature refusal`,
    );
    await stopRuntime();
  }
  const settings = join(folder, "settings.json");
  await writeFile(
    settings,
    JSON.stringify({
      vars: { ...env, DB_HOST: "127.0.0.1" },
      resources: {
        max_memory_bytes: 256 * 1024 * 1024,
        max_wall_time_ms: 15000,
        max_execution_time_ms: 20000,
      },
    }),
    { mode: 0o600 },
  );
  const worker = resolve(
    process.env.HIBANA_TEST_RUNTIME_BIN || "target/release/hibana-worker",
  );
  const deniedUrl = await launch(
    worker,
    [
      "--dev-component",
      artifact,
      "--dev-settings",
      settings,
      "--bind",
      "127.0.0.1:0",
    ],
    /Hibana \(Wasmtime\): (http:\/\/127\.0\.0\.1:\d+)/,
  );
  const response = await fetch(deniedUrl + "/", {
    signal: AbortSignal.timeout(25000),
  });
  assert.equal(response.status, 500, runtimeLog);
  const denied = await response.json();
  assert.equal(denied.code, "EACCES", JSON.stringify(denied));
  report.hibanaDevEgress = "denied (EACCES), unchanged";
  console.log(
    "PASS actual Hibana runtime still denies egress; packages never grant network permissions",
  );
  await stopRuntime();
}

async function checkDatabase() {
  const image = process.env.HIBANA_PG_IMAGE || "postgres:16";
  // Resolve the local image to an immutable ID before creating our own container.
  report.postgresImage = await run("docker", [
    "image",
    "inspect",
    image,
    "--format",
    "{{.Id}}",
  ]);
  await run("openssl", [
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-keyout",
    join(folder, "server.key"),
    "-out",
    join(folder, "server.crt"),
    "-days",
    "1",
    "-subj",
    "/CN=localhost",
    "-addext",
    "subjectAltName=DNS:localhost,IP:127.0.0.1",
    "-addext",
    "basicConstraints=critical,CA:FALSE",
  ]);
  // SASLprep mapping and NFKC must work in the guest engine too.
  const password = "Ａ\u00ad\u00a0" + randomBytes(24).toString("hex");
  const envFile = join(folder, "postgres.env");
  await writeFile(
    envFile,
    [
      "POSTGRES_DB=probe",
      "POSTGRES_USER=probe",
      `POSTGRES_PASSWORD=${password}`,
      // PostgreSQL still negotiates SCRAM for SCRAM-stored passwords under md5
      // HBA; the dedicated legacy role below stores an MD5 verifier instead.
      "POSTGRES_HOST_AUTH_METHOD=md5",
      "POSTGRES_INITDB_ARGS=--auth-host=md5",
      "",
    ].join("\n"),
    { mode: 0o600 },
  );
  await run("docker", [
    "run",
    "--rm",
    "-d",
    "--name",
    container,
    "--tmpfs",
    "/var/lib/postgresql/data",
    "--tmpfs",
    "/run/pg-cert",
    "--mount",
    `type=bind,source=${folder},target=/fixtures,readonly`,
    "--env-file",
    envFile,
    "-p",
    "127.0.0.1::5432",
    report.postgresImage,
    "/bin/bash",
    "-c",
    "install -o postgres -g postgres -m 600 /fixtures/server.key /run/pg-cert/server.key && exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/fixtures/server.crt -c ssl_key_file=/run/pg-cert/server.key",
  ]);
  started = true;
  const address = await run("docker", ["port", container, "5432/tcp"]);
  assert.match(address, /^127\.0\.0\.1:\d+$/);
  const config = {
    host: "127.0.0.1",
    port: Number(address.split(":")[1]),
    user: "probe",
    database: "probe",
    password,
    connectionTimeoutMillis: 5000,
    query_timeout: 5000,
  };
  for (let attempt = 0; ; attempt++) {
    try {
      await run("docker", [
        "exec",
        container,
        "pg_isready",
        "-h",
        "127.0.0.1",
        "-U",
        "probe",
        "-d",
        "probe",
      ]);
      break;
    } catch (error) {
      if (attempt >= 59) throw error;
      await sleep(500);
    }
  }
  // initdb's md5 HBA can create the initial owner with an MD5 verifier too.
  // Explicitly provision SCRAM for the main role; only probe_md5 is legacy.
  await runCommand(
    "docker",
    [
      "exec",
      "-i",
      container,
      "psql",
      "-U",
      "probe",
      "-d",
      "probe",
      "-v",
      "ON_ERROR_STOP=1",
    ],
    {
      input: Buffer.from(
        `SET password_encryption='scram-sha-256'; ALTER ROLE probe PASSWORD '${password.replaceAll("'", "''")}';\n`,
      ),
      timeoutMs: 30000,
    },
  );
  const ca = await readFile(join(folder, "server.crt"), "utf8");
  report.nativeNodeControl = [];
  for (const encrypted of [false, true]) {
    const client = new pg.Client({
      ...config,
      ssl: encrypted ? { ca, rejectUnauthorized: true } : false,
    });
    let scram = false;
    client.connection.on("authenticationSASL", (message) => {
      scram = message.mechanisms.includes("SCRAM-SHA-256");
    });
    try {
      await client.connect();
      if (!encrypted) {
        await client.query(
          await readFile(join(fixture, "migrations/0001_probe.sql"), "utf8"),
        );
        const verifier =
          "md5" +
          createHash("md5")
            .update(password + "probe_md5")
            .digest("hex");
        await client.query(
          `CREATE ROLE probe_md5 LOGIN PASSWORD '${verifier}'`,
        );
      }
      assert.equal(scram, true, "PostgreSQL must request SCRAM authentication");
      const result = await client.query(
        "SELECT $1::integer AS answer, $2::text AS greeting",
        [42, "こんにちは Hibana 🔥"],
      );
      assert.deepEqual(result.rows, [
        { answer: 42, greeting: "こんにちは Hibana 🔥" },
      ]);
      const {
        rows: [server],
      } = await client.query(
        "SELECT current_setting('server_version') AS version, ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
      );
      assert.equal(server.ssl, encrypted);
      if (encrypted) assert.equal(client.connection.stream.authorized, true);
      report.nativeNodeControl.push({
        transport: encrypted
          ? "TLS (verified custom CA, PostgreSQL SSLRequest)"
          : "TCP",
        authentication: "SCRAM-SHA-256",
        parameterizedQuery: "passed",
        unicode: "passed",
        serverVersion: server.version,
      });
      console.log(
        `PASS native Node pg control: ${encrypted ? "TLS" : "TCP"}, SCRAM, parameterized SELECT, UTF-8`,
      );
    } finally {
      await client.end();
    }
  }
  const legacy = new pg.Client({ ...config, user: "probe_md5" });
  let md5Requested = false;
  legacy.connection.on("authenticationMD5Password", () => {
    md5Requested = true;
  });
  try {
    await legacy.connect();
    assert.equal(md5Requested, true);
    assert.equal(
      (await legacy.query("SELECT 42 AS answer")).rows[0].answer,
      42,
    );
    report.nativeMd5Control = "passed";
  } finally {
    await legacy.end();
  }
  const invalid = new pg.Client({
    ...config,
    password: randomBytes(24).toString("hex"),
  });
  try {
    await assert.rejects(invalid.connect(), { code: "28P01" });
    report.nativeWrongPassword = "rejected (28P01)";
    console.log("PASS native Node pg control: wrong password rejected");
  } finally {
    await invalid.end();
  }
  await checkWasm(config, ca);
}

try {
  await mkdir(reportDir, { recursive: true });
  await buildApplication();
  await checkDatabase();
} catch (error) {
  console.error(runtimeLog);
  console.error("Failed request:", report.requests?.at(-1));
  throw error;
} finally {
  try {
    await stopRuntime();
    if (started) {
      await run("docker", ["stop", container]);
      report.temporaryDatabase =
        "stopped and removed (--rm, tmpfs, no persistent volume)";
    }
  } finally {
    await rm(folder, { recursive: true, force: true });
    await writeFile(
      join(reportDir, "report.json"),
      JSON.stringify(report, null, 2) + "\n",
    );
  }
}
console.log(`Report: ${reportDir}`);
console.log(
  "PASS PostgreSQL adapter and Drizzle acceptance complete; production egress policy unchanged.",
);
