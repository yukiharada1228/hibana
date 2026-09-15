// Packaged PostgreSQL adapter -> Hono -> Wasm -> disposable PostgreSQL.
// Only the trusted Wasmtime test fixture inherits network access.
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
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

const execute = promisify(execFile);
const root = fileURLToPath(new URL("..", import.meta.url));
const fixture = join(root, "scripts/fixtures/postgres");
const require = createRequire(join(fixture, "package.json"));
const pg = require("pg");
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
const artifact = join(application, ".hibana/build/app.wasm");

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
    "schema.mjs",
  ])
    await copyFile(join(fixture, name), join(application, name));
  await run(
    "npm",
    ["ci", "--offline", "--ignore-scripts", "--no-audit", "--no-fund"],
    { cwd: application },
  );
  const tarballs = [];
  report.extensions = [];
  for (const name of ["node-net", "postgres"]) {
    const [packed] = JSON.parse(
      await run(
        "npm",
        ["pack", "--ignore-scripts", "--json", "--pack-destination", folder],
        {
          cwd: join(root, "extensions", name),
        },
      ),
    );
    assert.ok(packed.files.some((file) => file.path.endsWith(".wasm")));
    assert.ok(
      packed.files.every(
        (file) =>
          !file.path.startsWith("component/") && file.path !== "build.mjs",
      ),
    );
    report.extensions.push({
      name,
      version: packed.version,
      integrity: packed.integrity,
    });
    tarballs.push(join(folder, packed.filename));
  }
  await run(
    "npm",
    [
      "install",
      "--offline",
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
        extensions: ["@hibana/node-net", "@hibana/postgres"],
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
  const url = await launch(
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
  async function call(path = "/", overrides = {}, status = 200) {
    const response = await fetch(url + path, {
      headers: {
        "x-hibana-env": Buffer.from(
          JSON.stringify({ ...env, ...overrides }),
        ).toString("base64url"),
      },
      signal: AbortSignal.timeout(35000),
    });
    const text = await response.text();
    assert.equal(response.status, status, `${path}: ${text}\n${runtimeLog}`);
    return JSON.parse(text);
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
  assert.deepEqual(await call("/options"), [
    "password",
    "channelBinding",
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
      "POSTGRES_HOST_AUTH_METHOD=scram-sha-256",
      "POSTGRES_INITDB_ARGS=--auth-host=scram-sha-256",
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
      if (!encrypted)
        await client.query(
          await readFile(join(fixture, "migrations/0001_probe.sql"), "utf8"),
        );
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
