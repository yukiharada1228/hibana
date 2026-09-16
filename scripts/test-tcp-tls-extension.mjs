// Real tarball -> Hono -> composed Wasm -> TCP/TLS fixtures, with no platform DB.
// WASMTIME_BIN points to Wasmtime 36.0.14. The isolated trusted fixture alone
// inherits network access; Hibana's production/dev security policy is unchanged.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import net from "node:net";
import tls from "node:tls";
import { X509Certificate } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  copyFile,
  rm,
  access,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { runCommand } from "./bounded-process.mjs";
import { resolveExtensions } from "../sdk/src/extensions.mjs";
import { packExtensions } from "./pack-extensions.mjs";

process.chdir(fileURLToPath(new URL("..", import.meta.url)));
const folder = await mkdtemp(join(tmpdir(), "hibana-tcp-tls-"));
const wasmtime = resolve(process.env.WASMTIME_BIN || "wasmtime");
const worker = resolve(
  process.env.HIBANA_TEST_RUNTIME_BIN || "target/release/hibana-worker",
);
const cp = resolve(
  process.env.HIBANA_TEST_CP_BIN || "target/debug/hibana-control-plane",
);
const sockets = new Set(),
  servers = [];
let child,
  logs = "";
const stop = async () => {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null)
    return;
  const ended = new Promise((done) => child.once("exit", done));
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 2000);
  await ended;
  clearTimeout(timer);
};
async function listen(server) {
  servers.push(server);
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("error", () => {});
    socket.on("close", () => sockets.delete(socket));
  });
  await new Promise((done, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", done);
  });
  return server.address().port;
}
async function launch(executable, args, pattern) {
  logs = "";
  child = spawn(executable, args, { stdio: ["ignore", "pipe", "pipe"] });
  let spawnError;
  child.on("error", (error) => {
    spawnError = error;
  });
  for (const stream of [child.stdout, child.stderr])
    stream.on("data", (bytes) => {
      logs = (logs + bytes).slice(-65536);
    });
  for (let n = 0; n < 1200; n++) {
    if (spawnError) throw spawnError;
    assert.equal(child.exitCode, null, logs);
    assert.equal(child.signalCode, null, logs);
    const match = logs.match(pattern);
    if (match) return match[1];
    await sleep(100);
  }
  throw new Error("Runtime startup deadline: " + logs);
}
try {
  for (const binary of [wasmtime, worker, cp]) await access(binary);
  const packages = await packExtensions(folder, ["node-tls"]);
  const application = join(folder, "application");
  await mkdir(application);
  await writeFile(
    join(application, "package.json"),
    JSON.stringify({
      private: true,
      type: "module",
      dependencies: {
        ...Object.fromEntries(
          packages.map((pkg) => [pkg.name, `file:${pkg.tarball}`]),
        ),
        hono: "4.13.7",
      },
    }),
  );
  await writeFile(
    join(application, "hibana.json"),
    JSON.stringify({
      name: "tcp-tls-test",
      main: "app.mjs",
      extensions: ["@hibana/node-tls"],
      limits: { timeout_ms: 30000 },
    }),
  );
  await copyFile(
    resolve("scripts/fixtures/network/app.mjs"),
    join(application, "app.mjs"),
  );
  await runCommand(
    "npm",
    ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund"],
    { cwd: application },
  );
  await assert.rejects(
    access(join(application, "node_modules/@hibana/node-tls/component")),
  );
  console.log(
    "PASS real extension tarball installs without Rust sources or install hooks",
  );
  // All packages may be installed; only explicit extension dependencies are enabled.
  for (const name of [
    "node-buffer",
    "node-events",
    "node-process",
    "node-stream",
  ]) {
    const plan = await resolveExtensions({
      root: application,
      main: "app.mjs",
      extensions: [`@hibana/${name}`],
    });
    assert.deepEqual(plan.components, []);
    assert.deepEqual(plan.permissions, []);
  }
  const tcpPlan = await resolveExtensions({
    root: application,
    main: "tcp-only.mjs",
    extensions: ["@hibana/tcp"],
  });
  assert.deepEqual(tcpPlan.imports, ["hibana:tcp/api@0.5.0"]);
  assert.equal(tcpPlan.components.length, 1);
  assert.equal(tcpPlan.aliases["node:tls"], undefined);
  const jco = resolve("sdk/node_modules/@bytecodealliance/jco/src/jco.js");
  const tlsWit = await runCommand(process.execPath, [
    jco,
    "wit",
    resolve("extensions/tls/dist/tls.wasm"),
  ]);
  assert.doesNotMatch(tlsWit, /import wasi:sockets/);
  const tcpWit = await runCommand(process.execPath, [
    jco,
    "wit",
    tcpPlan.components[0],
  ]);
  assert.match(tcpWit, /import wasi:sockets/);
  assert.doesNotMatch(tcpWit, /ip-name-lookup|node-tls|rustls/);
  assert.deepEqual(tcpPlan.aliases, {});
  assert.deepEqual(tcpPlan.preload, []);
  await copyFile(
    resolve("scripts/fixtures/network/tcp-only.mjs"),
    join(application, "tcp-only.mjs"),
  );
  await writeFile(
    join(application, "tcp-only.json"),
    JSON.stringify({
      name: "tcp-only",
      main: "tcp-only.mjs",
      extensions: ["@hibana/tcp"],
    }),
  );
  await runCommand(
    process.execPath,
    [resolve("sdk/src/cli.mjs"), "build", "--config", "tcp-only.json"],
    { cwd: application, timeoutMs: 180000 },
  );
  const tcpArtifact = join(folder, "tcp-only.wasm");
  await copyFile(join(application, ".hibana/build/app.wasm"), tcpArtifact);
  const tcpBytes = (await readFile(tcpArtifact)).byteLength;
  console.log(
    "PASS TCP-only composition excludes TLS; TLS engine has no socket imports",
  );

  // Capture build stderr too, since toolchain errors are useful diagnostics.
  const buildLog = await new Promise((done, reject) => {
    const build = spawn(
      process.execPath,
      [resolve("sdk/src/cli.mjs"), "build"],
      { cwd: application, stdio: ["ignore", "pipe", "pipe"] },
    );
    let output = "";
    const timer = setTimeout(() => build.kill("SIGKILL"), 180000);
    build.on("error", reject);
    for (const stream of [build.stdout, build.stderr])
      stream.on("data", (chunk) => {
        output = (output + chunk).slice(-65536);
      });
    build.on("close", (code) => {
      clearTimeout(timer);
      code === 0 ? done(output) : reject(new Error(output));
    });
  });
  assert.match(buildLog, /outbound-network/);
  const artifact = join(application, ".hibana/build/app.wasm");
  const tlsBytes = (await readFile(artifact)).byteLength;
  assert.ok(tcpBytes < tlsBytes);
  console.log(
    `Artifact sizes: TCP only ${tcpBytes} bytes; TCP + TLS ${tlsBytes} bytes`,
  );
  const accepted = JSON.parse(
    await runCommand(cp, ["--validate-stdin"], {
      input: await readFile(artifact),
      timeoutMs: 120000,
    }),
  );
  assert.ok(accepted.Ok, JSON.stringify(accepted));
  assert.ok(
    accepted.Ok.approved_imports.every((name) => name.startsWith("wasi:")),
  );
  console.log(
    "PASS ordinary CLI composition + control-plane validation; only standard WASI host imports",
  );

  await runCommand("openssl", [
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-keyout",
    join(folder, "key.pem"),
    "-out",
    join(folder, "ca.pem"),
    "-days",
    "1",
    "-subj",
    "/CN=localhost",
    "-addext",
    "subjectAltName=DNS:localhost",
    "-addext",
    "basicConstraints=critical,CA:FALSE",
  ]);
  const cert = await readFile(join(folder, "ca.pem"), "utf8"),
    key = await readFile(join(folder, "key.pem"));
  const echo = (socket) => {
    socket.on("error", () => {});
    socket.pipe(socket);
  };
  const tcpPort = await listen(net.createServer(echo));
  const tcpAddress = await launch(
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
    /Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/i,
  );
  const tcpResponse = await fetch(tcpAddress + "/", {
    method: "POST",
    body: JSON.stringify({ port: tcpPort }),
    signal: AbortSignal.timeout(35000),
  });
  assert.equal(tcpResponse.status, 200, logs);
  assert.deepEqual(await tcpResponse.json(), {
    received: "TCP only: 日本語🔥",
  });
  await stop();
  console.log("PASS TCP-only Wasm performs real network I/O without TLS");
  const tlsServer = tls.createServer(
    { cert, key, ALPNProtocols: ["echo/1"] },
    echo,
  );
  tlsServer.on("tlsClientError", () => {});
  const tlsPort = await listen(tlsServer);
  const idlePort = await listen(net.createServer());
  // TCP succeeds but the peer closes as soon as it receives ClientHello.
  const eofPort = await listen(
    net.createServer((socket) => {
      socket.once("data", () => socket.end());
    }),
  );
  const startEofPort = await listen(
    net.createServer((socket) => {
      socket.once("data", (bytes) => {
        assert.equal(bytes.toString(), "STARTTLS\n");
        socket.write("READY\n");
        socket.once("data", () => socket.end());
      });
    }),
  );
  const slowPort = await listen(
    net.createServer((socket) => {
      socket.pause();
      setTimeout(() => {
        echo(socket);
        socket.resume();
      }, 100);
    }),
  );
  const context = tls.createSecureContext({ cert, key });
  const startPort = await listen(
    net.createServer((socket) => {
      socket.once("data", (bytes) => {
        assert.equal(bytes.toString(), "STARTTLS\n");
        socket.write("READY\n", () =>
          echo(
            new tls.TLSSocket(socket, {
              isServer: true,
              secureContext: context,
            }),
          ),
        );
      });
    }),
  );
  const passivePorts = {},
    bufferedPorts = {};
  const bufferedText = "b".repeat(512 * 1024);
  for (const transport of ["tcp", "tls", "starttls"])
    for (const [ports, text] of [
      [passivePorts, ""],
      [bufferedPorts, bufferedText],
    ]) {
      // Let Node finish its handshake callback before requesting close_notify.
      const send = (socket) => setImmediate(() => socket.end(text));
      let server;
      if (transport === "tcp") server = net.createServer(send);
      else if (transport === "tls") {
        server = tls.createServer({ cert, key }, send);
        server.on("tlsClientError", () => {});
      } else {
        server = net.createServer((socket) => {
          socket.once("data", (bytes) => {
            assert.equal(bytes.toString(), "STARTTLS\n");
            socket.write("READY\n", () => {
              const secure = new tls.TLSSocket(socket, {
                isServer: true,
                secureContext: context,
              });
              secure.on("error", () => {});
              secure.once("secure", () => send(secure));
            });
          });
        });
      }
      ports[transport] = await listen(server);
    }
  const unused = net.createServer();
  const refusedPort = await listen(unused);
  await new Promise((done) => unused.close(done));
  const address = await launch(
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
    /Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/i,
  );
  const call = async (body) => {
    const response = await fetch(address + "/", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(35000),
    });
    const text = await response.text();
    assert.equal(
      response.status,
      200,
      `${body.test} (${body.transport || "default"}, halfOpen=${body.halfOpen ?? false}): ${text}\n${logs}`,
    );
    return JSON.parse(text);
  };
  const small = "日本語 🔥\n";
  for (const host of ["127.0.0.1", "localhost"])
    assert.equal(
      (await call({ test: "tcp", host, port: tcpPort, text: small })).received,
      small,
    );
  console.log(
    "PASS real TCP echo, DNS/address fallback and UTF-8 stream decoding",
  );
  for (const transport of ["tcp", "tls", "starttls"]) {
    for (const halfOpen of [false, true]) {
      const results = await call({
        test: "passive-eof",
        transport,
        halfOpen,
        port: passivePorts[transport],
        ca: cert,
      });
      assert.equal(results.length, 40);
      for (const result of results) {
        assert.deepEqual(result.events, [
          "connect",
          ...(transport === "tcp" ? [] : ["secureConnect"]),
          "end",
          "finish",
          "close",
        ]);
        assert.equal(result.destroyed, true);
        assert.equal(result.rawClosed, true);
        if (halfOpen) assert.equal(result.writableAtEnd, true);
      }
    }
    const [buffered] = await call({
      test: "delayed-read",
      transport,
      port: bufferedPorts[transport],
      ca: cert,
    });
    assert.ok(
      buffered.buffered >= 1024 && buffered.buffered <= 1024 + 16 * 1024,
    );
    assert.equal(buffered.bytesBeforeRead, buffered.buffered);
    assert.equal(buffered.received, bufferedText);
    assert.equal(buffered.destroyed, true);
    assert.equal(buffered.rawClosed, true);
  }
  console.log(
    "PASS passive TCP/TLS/STARTTLS EOF, half-open shutdown and resource reuse across 240 connections without data listeners",
  );
  console.log(
    "PASS delayed reads preserve 512 KiB payloads and bounded readable backpressure in TCP/TLS/STARTTLS",
  );
  const large = "a".repeat(512 * 1024);
  const backpressure = await call({ test: "tcp", port: slowPort, text: large });
  assert.equal(backpressure.received, large);
  assert.equal(backpressure.backpressure, true);
  assert.equal(backpressure.drained, true);
  assert.equal(backpressure.bytesRead, large.length);
  assert.equal(backpressure.bytesWritten, large.length);
  console.log(
    "PASS 512 KiB TCP transfer, bounded writes and drain backpressure",
  );
  const secure = await call({
    test: "tls",
    port: tlsPort,
    text: large,
    ca: cert,
  });
  assert.equal(secure.received, large);
  assert.equal(secure.authorized, true);
  assert.equal(secure.alpn, "echo/1");
  assert.equal(
    secure.certificate,
    new X509Certificate(cert).raw.toString("base64"),
  );
  console.log(
    "PASS Rust/Wasm TLS with custom CA, hostname verification, ALPN and large transfer",
  );
  for (const options of [{}, { ca: cert, servername: "wrong.example" }]) {
    const rejected = await call({
      test: "tls-failure",
      port: tlsPort,
      ...options,
    });
    assert.equal(rejected.code, "ERR_TLS_CONNECTION");
    assert.match(rejected.message, /certificate/i);
  }
  console.log("PASS untrusted certificates and wrong hostname fail closed");
  for (const ca of ["", "not a certificate"])
    assert.equal(
      (await call({ test: "tls-failure", port: tlsPort, ca })).code,
      "EINVAL",
    );
  console.log("PASS empty or malformed CA never falls back to default trust");
  const upgraded = await call({
    test: "starttls",
    port: startPort,
    ca: cert,
    text: small,
  });
  assert.equal(upgraded.received, small);
  assert.equal(upgraded.authorized, true);
  assert.equal(
    upgraded.certificate,
    new X509Certificate(cert).raw.toString("base64"),
  );
  assert.equal(upgraded.rawClosed, true);
  console.log("PASS STARTTLS ownership transfer and socket cleanup");
  for (const upgrade of [false, true]) {
    const result = await call({
      test: "starttls-options",
      upgrade,
      port: upgrade ? startPort : tcpPort,
      text: small,
      ca: cert,
    });
    assert.equal(result.received, small);
    assert.deepEqual(result.rejected, Array(10).fill("ERR_OUT_OF_RANGE"));
    if (upgrade) {
      assert.equal(result.authorized, true);
      assert.equal(result.rawClosed, true);
    }
  }
  console.log(
    "PASS invalid STARTTLS timeouts preserve TCP echo and allow a later verified TLS upgrade",
  );
  for (const starttls of [false, true]) {
    const failures = await call({
      test: "tls-eof",
      starttls,
      port: starttls ? startEofPort : eofPort,
    });
    assert.equal(failures.length, 40);
    for (const failure of failures)
      assert.deepEqual(failure, {
        errors: ["ECONNRESET"],
        secured: false,
        destroyed: true,
        rawClosed: true,
      });
  }
  console.log(
    "PASS TLS/STARTTLS handshake EOF errors once, closes sockets and releases all resources across 80 failures",
  );
  assert.deepEqual(await call({ test: "timeout", port: idlePort }), {
    closedByTimeout: false,
    closedAfterDestroy: true,
  });
  assert.deepEqual(await call({ test: "tls-timeout", port: idlePort }), {
    closedByTimeout: false,
    closedAfterDestroy: true,
  });
  assert.equal(
    (await call({ test: "tcp-failure", port: refusedPort })).code,
    "ECONNREFUSED",
  );
  const parallel = await call({ test: "parallel", port: slowPort });
  assert.equal(parallel.timerRanBeforeCompletion, true);
  parallel.sockets.forEach((socket, n) =>
    assert.equal(socket.received, `${n}:日本語🔥`),
  );
  assert.deepEqual(await call({ test: "repeated", port: tcpPort }), {
    count: 80,
  });
  assert.equal((await call({ test: "limit", port: idlePort })).code, "EMFILE");
  const cancelled = await call({ test: "cancel", port: idlePort });
  assert.equal(cancelled.destroyed, true);
  assert.equal(cancelled.closes, 1);
  assert.ok(cancelled.errors <= 1);
  const options = await call({ test: "options" });
  assert.deepEqual(options.ips, [4, 6, 0, 0]);
  assert.ok(options.rejected.every((code) => typeof code === "string"));
  console.log(
    "PASS timeout, refusal, cancellation, concurrent sockets/timers, repeated resource disposal and unsupported API errors",
  );
  await stop();

  // An installed package declares a requirement; it never grants host access.
  const settings = join(folder, "settings.json");
  await writeFile(
    settings,
    JSON.stringify({
      vars: {},
      resources: {
        max_memory_bytes: 256 * 1024 * 1024,
        max_wall_time_ms: 15000,
        max_execution_time_ms: 20000,
      },
    }),
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
    method: "POST",
    body: JSON.stringify({ test: "tcp-failure", port: tcpPort }),
    signal: AbortSignal.timeout(20000),
  });
  assert.equal(response.status, 200, logs);
  const denied = await response.json();
  assert.equal(denied.code, "EACCES", JSON.stringify(denied));
  console.log(
    "PASS actual Hibana runtime denies outbound access despite the installed TCP/TLS package",
  );
  await stop();
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  await stop();
  for (const socket of sockets) socket.destroy();
  await Promise.all(
    servers
      .filter((server) => server.listening)
      .map((server) => new Promise((done) => server.close(done))),
  );
  await rm(folder, { recursive: true, force: true });
}
