// Verify the distributed feature boundaries, not just source-directory separation.
import assert from "node:assert/strict";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
  realpath,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";
import { setTimeout as sleep } from "node:timers/promises";
import { packExtensions } from "./pack-extensions.mjs";
import { resolveExtensions } from "../sdk/src/extensions.mjs";
import { writePostgresSelection } from "./postgres-selection.mjs";
const root = fileURLToPath(new URL("..", import.meta.url));
const { build: bundleJavaScript } = createRequire(
  join(root, "sdk/package.json"),
)("esbuild");
const folder = await mkdtemp(join(tmpdir(), "hibana-feature-boundaries-"));
const application = join(folder, "application");
const execute = promisify(execFile);
const wasmtime = process.env.WASMTIME_BIN || "wasmtime";
const expectedStreams = {
  text: "HELLO STREAM 日本語 🔥",
  notifications: 1,
  inherited: true,
  separateModes: [true, false, 3, 1],
  operators: "undefined",
  finished: true,
  error: "ERR_STREAM_FIXTURE",
  released: true,
};
const barrelSource = `
import {Readable} from "./stream-api.mjs";
export default {async fetch() {
  const stream = new Readable({read() {
    this.push(new Uint8Array([0, 65, 66, 0]).subarray(1, 3));
    this.push(" 日本語 🔥");
    this.push(null);
  }});
  let text = "";
  for await (const chunk of stream) text += chunk.toString();
  return Response.json({text, operators: typeof stream.map, released: stream.destroyed});
}};`;
const expectedBarrel = {
  text: "AB 日本語 🔥",
  operators: "undefined",
  released: true,
};
const writableSource = `
import {Writable} from "./stream-api.mjs";
export default {async fetch() {
  let text = "", finished = 0;
  const stream = new Writable({highWaterMark: 1, write(chunk, encoding, done) {
    setTimeout(() => {text += chunk.toString(); done();}, 1);
  }});
  const closed = new Promise((resolve, reject) => {
    stream.on("finish", () => finished++);
    stream.once("error", reject);
    stream.once("close", resolve);
  });
  const backpressure = !stream.write(new Uint8Array([0, 65, 66, 0]).subarray(1, 3));
  stream.end(" 日本語 🔥");
  await closed;
  return Response.json({text, finished, backpressure, released: stream.destroyed});
}};`;
const expectedWritable = {
  text: "AB 日本語 🔥",
  finished: 1,
  backpressure: true,
  released: true,
};
let child,
  logs = "";

async function verifyStreamSelection() {
  async function bundle(source, extension) {
    const plan = await resolveExtensions({
      root: application,
      main: "app.mjs",
      extensions: [`@hibana/${extension}`],
    });
    const result = await bundleJavaScript({
      stdin: {
        contents: [
          ...plan.preload.map((path) => `import ${JSON.stringify(path)};`),
          source,
        ].join("\n"),
        resolveDir: application,
      },
      absWorkingDir: application,
      bundle: true,
      platform: "browser",
      format: "esm",
      target: "es2022",
      mainFields: ["module", "main"],
      conditions: ["import", "default"],
      alias: plan.aliases,
      external: plan.imports,
      metafile: true,
      write: false,
    });
    return {
      code: result.outputFiles[0].text,
      bytes: result.outputFiles[0].contents.length,
      inputs: Object.keys(result.metafile.inputs),
      emittedInputs: Object.entries(
        Object.values(result.metafile.outputs)[0].inputs,
      )
        .filter(([, value]) => value.bytesInOutput > 0)
        .map(([path]) => path),
    };
  }
  const optional = [
    "transform",
    "passthrough",
    "pipeline",
    "compose",
    "operators",
  ].map((name) => `readable-stream/lib/internal/streams/${name}.js`);
  optional.push(
    "readable-stream/lib/stream/promises.js",
    "readable-stream/lib/ours/browser.js",
  );
  const socket = await bundle('export {Socket} from "node:net";', "node-net");
  const fullSocket = await bundle(
    'import "node:stream"; export {Socket} from "node:net";',
    "node-net",
  );
  for (const path of optional) {
    assert.ok(!socket.inputs.some((input) => input.endsWith(path)), path);
    assert.ok(
      fullSocket.inputs.some((input) => input.endsWith(path)),
      path,
    );
  }
  assert.ok(socket.bytes < fullSocket.bytes);

  const minimal = await bundle(
    'export {Duplex} from "@hibana/node-stream/duplex";',
    "node-stream",
  );
  for (const path of optional)
    assert.ok(!minimal.inputs.some((input) => input.endsWith(path)), path);
  let bundleNumber = 0;
  const load = async (code) => {
    const path = join(application, `stream-test-${bundleNumber++}.mjs`);
    await writeFile(path, code);
    return import(pathToFileURL(path).href);
  };
  const { Duplex } = await load(minimal.code);
  const events = { end: 0, finish: 0, close: 0 };
  const echo = new Duplex({
    allowHalfOpen: false,
    highWaterMark: 1,
    read() {},
    write(chunk, encoding, done) {
      this.push(chunk);
      done();
    },
    final(done) {
      this.push(null);
      done();
    },
  });
  for (const name of Object.keys(events)) echo.on(name, () => events[name]++);
  const closed = new Promise((done, reject) => {
    echo.once("close", done);
    echo.once("error", reject);
  });
  const received = (async () => {
    const chunks = [];
    for await (const chunk of echo) chunks.push(Buffer.from(chunk));
    return Buffer.concat(chunks).toString();
  })();
  echo.write("こんにちは");
  echo.write(new Uint8Array([0, 65, 66, 0]).subarray(1, 3));
  echo.end(" 🔥");
  assert.equal(await received, "こんにちはAB 🔥");
  await closed;
  assert.deepEqual(events, { end: 1, finish: 1, close: 1 });
  assert.equal(echo.destroyed, true);

  // Inspect each published entry independently, not a single bundle containing
  // all features. Shared prerequisites are allowed; unrelated APIs are not.
  const entries = [
    ["readable", "Readable", []],
    ["writable", "Writable", []],
    ["duplex", "Duplex", []],
    ["transform", "Transform", ["transform"]],
    ["passthrough", "PassThrough", ["transform", "passthrough"]],
    ["pipeline", "pipeline", ["transform", "passthrough", "pipeline"]],
    ["compose", "compose", ["transform", "passthrough", "pipeline", "compose"]],
    ["finished", "finished", []],
  ];
  const modules = new Map();
  const sizes = {};
  function assertStreamClasses(inputs, entry) {
    const expected =
      entry === "finished"
        ? []
        : ["readable", "writable"].includes(entry)
          ? [entry]
          : ["readable", "writable", "duplex"];
    for (const name of ["readable", "writable", "duplex"])
      assert.equal(
        inputs.some((path) => path.endsWith(`/streams/${name}.js`)),
        expected.includes(name),
        `${entry}: ${name} implementation`,
      );
  }
  await writeFile(
    join(application, "stream-api.mjs"),
    entries
      .map(
        ([entry, name]) =>
          `export {${name}} from "@hibana/node-stream/${entry}";`,
      )
      .join("\n"),
  );
  for (const [entry, name, required] of entries) {
    const selected = await bundle(
      `export {default, ${name}} from "@hibana/node-stream/${entry}";`,
      "node-stream",
    );
    assertStreamClasses(selected.emittedInputs, entry);
    for (const path of optional) {
      const present = selected.inputs.some((input) => input.endsWith(path));
      const expected = required.some((name) =>
        path.endsWith(`/streams/${name}.js`),
      );
      assert.equal(present, expected, `${entry}: ${path}`);
    }
    if (entry === "finished") {
      assert.ok(
        !selected.inputs.some((path) =>
          /\/streams\/(readable|writable|duplex)\.js$/.test(path),
        ),
      );
      assert.ok(
        !selected.inputs.some((path) =>
          path.endsWith("/node-stream/src/bytes.mjs"),
        ),
      );
    }
    const api = await load(selected.code);
    assert.equal(typeof api.default, "function", entry);
    assert.equal(api.default, api[name], entry);
    modules.set(entry, api.default);
    sizes[entry] = selected.bytes;

    // An application may re-export several APIs from a shared file. Inspect
    // emitted code, not parsed inputs: unused modules can still be parsed.
    const barrel = await bundle(
      `export {${name}} from "./stream-api.mjs";`,
      "node-stream",
    );
    assertStreamClasses(barrel.emittedInputs, entry);
    for (const path of optional) {
      assert.equal(
        barrel.emittedInputs.some((input) => input.endsWith(path)),
        required.some((name) => path.endsWith(`/streams/${name}.js`)),
        `${entry} via re-export: ${path}`,
      );
    }
    if (entry === "finished")
      assert.ok(
        !barrel.emittedInputs.some((path) =>
          /\/streams\/(readable|writable|duplex)\.js$/.test(path),
        ),
      );
    assert.equal(
      barrel.emittedInputs.some((path) =>
        path.endsWith("/node-stream/src/bytes.mjs"),
      ),
      entry !== "finished",
      `${entry}: byte initialization`,
    );
    assert.equal(typeof (await load(barrel.code))[name], "function", entry);
  }
  const barrel = await bundle(barrelSource, "node-stream");
  const barrelApp = await load(barrel.code);
  const barrelResponse = await barrelApp.default.fetch();
  assert.deepEqual(await barrelResponse.json(), expectedBarrel);
  const writableApp = await load(
    (await bundle(writableSource, "node-stream")).code,
  );
  assert.deepEqual(
    await (await writableApp.default.fetch()).json(),
    expectedWritable,
  );
  console.log(
    "PASS all selective stream entries exclude unused APIs through shared re-exports; byte initialization is retained",
  );
  const initialized = await bundle(
    'import "node:stream"; export {Readable} from "@hibana/node-stream/readable";',
    "node-stream",
  );
  assert.equal(
    typeof (await load(initialized.code)).Readable.prototype.map,
    "function",
    "side-effect-only import of the full stream API retains its initialization",
  );
  // Loading Duplex later must enable its per-side options without changing
  // existing single-direction streams or creating a second class hierarchy.
  const lazy = await load(
    (
      await bundle(
        `export {Readable} from "@hibana/node-stream/readable";
         export {Writable} from "@hibana/node-stream/writable";
         export const loadDuplex = () => import("@hibana/node-stream/duplex");`,
        "node-stream",
      )
    ).code,
  );
  const readOnly = new lazy.Readable({ readableObjectMode: true });
  const writeOnly = new lazy.Writable({ writableObjectMode: true });
  assert.equal(readOnly.readableObjectMode, false);
  assert.equal(writeOnly.writableObjectMode, false);
  const { Duplex: LateDuplex } = await lazy.loadDuplex();
  class MixedStream extends LateDuplex {}
  const splitOptions = {
    readableObjectMode: true,
    writableObjectMode: false,
    readableHighWaterMark: 3,
    writableHighWaterMark: 7,
  };
  const mixed = new MixedStream(splitOptions);
  assert.ok(mixed instanceof lazy.Readable);
  assert.ok(mixed instanceof lazy.Writable);
  assert.equal(mixed.readableObjectMode, true);
  assert.equal(mixed.writableObjectMode, false);
  assert.equal(mixed.readableHighWaterMark, 3);
  assert.equal(mixed.writableHighWaterMark, 7);
  assert.equal(
    new lazy.Readable.ReadableState(splitOptions, mixed).objectMode,
    true,
  );
  assert.equal(
    new lazy.Writable.WritableState(
      { writableObjectMode: true },
      Object.create(LateDuplex.prototype),
    ).objectMode,
    true,
  );
  assert.equal(readOnly.readableObjectMode, false);
  assert.equal(writeOnly.writableObjectMode, false);
  for (const stream of [readOnly, writeOnly, mixed]) stream.destroy();
  console.log(
    "PASS Readable and Writable exclude each other and Duplex; late Duplex loading preserves inheritance and per-side options",
  );
  const collect = async (stream) => {
    const chunks = [];
    for await (const chunk of stream) chunks.push(Buffer.from(chunk));
    return Buffer.concat(chunks).toString();
  };
  assert.equal(
    await collect(modules.get("readable").from(["日本語 ", "🔥"])),
    "日本語 🔥",
  );
  let written = "";
  const writable = new (modules.get("writable"))({
    write(chunk, encoding, done) {
      written += chunk;
      done();
    },
  });
  let cleanup;
  const observed = new Promise((resolve, reject) => {
    cleanup = modules.get("finished")(writable, (error) =>
      error ? reject(error) : resolve(),
    );
  });
  writable.end(new Uint8Array([0, 97, 98, 0]).subarray(1, 3));
  await observed;
  cleanup();
  assert.equal(written, "ab");
  for (const entry of ["transform", "passthrough"]) {
    const stream = new (modules.get(entry))({
      ...(entry === "transform"
        ? {
            transform(chunk, encoding, done) {
              done(null, chunk.toString().toUpperCase());
            },
          }
        : {}),
      highWaterMark: 1,
    });
    const output = collect(stream);
    stream.end(new Uint8Array([0, 97, 98, 0]).subarray(1, 3));
    assert.equal(await output, entry === "transform" ? "AB" : "ab");
  }
  const piped = await new Promise((resolve, reject) =>
    modules.get("pipeline")(
      ["a", "b"],
      async function* (input) {
        for await (const value of input) yield value.toUpperCase();
      },
      async (input) => {
        let text = "";
        for await (const value of input) text += value;
        return text;
      },
      (error, value) => (error ? reject(error) : resolve(value)),
    ),
  );
  assert.equal(piped, "AB");
  const composed = modules.get("compose")(async function* (input) {
    for await (const value of input) yield value.toString().toUpperCase();
  });
  const composedOutput = collect(composed);
  composed.end("ab");
  assert.equal(await composedOutput, "AB");

  // Resolve imports against the installed consumer tarballs, not the checkout.
  const fixtureSource = await readFile(
    join(root, "scripts/fixtures/network/streams.mjs"),
    "utf8",
  );
  const selective = await bundle(fixtureSource, "node-stream");
  assert.deepEqual(
    await (await load(selective.code)).verify(),
    expectedStreams,
  );
  const fullPass = await bundle(
    'export {PassThrough} from "node:stream";',
    "node-stream",
  );
  assert.ok(sizes.passthrough < fullPass.bytes);
  console.log(
    `PASS selective stream entries: ${JSON.stringify(sizes)}; PassThrough full entry ${fullPass.bytes} bytes`,
  );

  const full = await bundle(
    `
    ${entries.map(([entry, name]) => `export {${name} as selected_${name}} from "@hibana/node-stream/${entry}";`).join("\n")}
    export {Duplex, Readable, Writable, Transform, PassThrough, pipeline, compose} from "node:stream";
    export {finished} from "node:stream";
  `,
    "node-stream",
  );
  const api = await load(full.code);
  assert.equal(typeof api.Readable.prototype.map, "function");
  for (const [, name] of entries)
    assert.equal(
      api[`selected_${name}`],
      api[name],
      `${name}: shared implementation`,
    );
  const pipeline = promisify(api.pipeline);
  let text = "";
  await pipeline(
    api.Readable.from(["hello", " stream"]),
    api.compose(
      new api.PassThrough(),
      new api.Transform({
        transform(chunk, encoding, done) {
          done(null, chunk.toString().toUpperCase());
        },
      }),
    ),
    new api.Writable({
      write(chunk, encoding, done) {
        text += chunk;
        done();
      },
    }),
  );
  assert.equal(text, "HELLO STREAM");
  const failure = new Error("stream fixture failure");
  const source = api.Readable.from(["input"]);
  const broken = new api.Transform({
    transform(chunk, encoding, done) {
      done(failure);
    },
  });
  const sink = new api.Writable({
    write(chunk, encoding, done) {
      done();
    },
  });
  await assert.rejects(
    pipeline(source, broken, sink),
    (error) => error === failure,
  );
  assert.ok([source, broken, sink].every((stream) => stream.destroyed));
  console.log(
    `PASS Socket JS excludes optional stream operators: ${fullSocket.bytes} -> ${socket.bytes} bytes; Duplex lifecycle and full stream interoperability work`,
  );
}
async function run(command, args, cwd = root) {
  try {
    return (
      await execute(command, args, {
        cwd,
        timeout: 180000,
        killSignal: "SIGKILL",
        maxBuffer: 4 * 1024 * 1024,
      })
    ).stdout;
  } catch (error) {
    console.error(error.stderr || error.message);
    throw error;
  }
}
async function stop() {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const ended = new Promise((done) => child.once("exit", done));
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 2000);
  await ended;
  clearTimeout(timer);
}
async function request(
  extension,
  source,
  { network = false, components = 1 } = {},
) {
  await writeFile(join(application, "app.mjs"), source);
  await writeFile(
    join(application, "hibana.json"),
    JSON.stringify({
      name: "feature-probe",
      main: "app.mjs",
      extensions: [`@hibana/${extension}`],
    }),
  );
  const plan = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: [`@hibana/${extension}`],
  });
  assert.equal(plan.components.length, components, extension);
  if (components) {
    assert.deepEqual(plan.aliases, {});
    assert.deepEqual(plan.preload, []);
  } else assert.deepEqual(plan.permissions, []);
  await run(
    process.execPath,
    [join(root, "sdk/src/cli.mjs"), "build"],
    application,
  );
  logs = "";
  child = spawn(
    wasmtime,
    [
      "serve",
      "--addr",
      "127.0.0.1:0",
      "-S",
      `cli=y,inherit-network=${network ? "y" : "n"},allow-ip-name-lookup=${network ? "y" : "n"},tcp=n,udp=n`,
      "-W",
      "timeout=15s",
      join(application, ".hibana/build/app.wasm"),
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let failure;
  child.on("error", (error) => {
    failure = error;
  });
  for (const stream of [child.stdout, child.stderr])
    stream.on("data", (bytes) => {
      logs = (logs + bytes).slice(-65536);
    });
  let address;
  for (let attempt = 0; attempt < 1200; attempt++) {
    if (failure) throw failure;
    assert.equal(child.exitCode, null, logs);
    assert.equal(child.signalCode, null, logs);
    address = logs.match(/Serving HTTP on (http:\/\/127\.0\.0\.1:\d+)/)?.[1];
    if (address) break;
    await sleep(100);
  }
  assert.ok(address, logs);
  try {
    const response = await fetch(address, {
      signal: AbortSignal.timeout(20000),
    });
    const text = await response.text();
    assert.equal(response.status, 200, text + logs);
    return JSON.parse(text);
  } finally {
    await stop();
  }
}
try {
  const packages = await packExtensions(folder);
  await mkdir(application);
  await writeFile(
    join(application, "package.json"),
    JSON.stringify({
      private: true,
      type: "module",
      dependencies: Object.fromEntries(
        packages.map((pkg) => [pkg.name, `file:${pkg.tarball}`]),
      ),
    }),
  );
  await run(
    "npm",
    ["install", "--ignore-scripts", "--no-audit", "--no-fund"],
    application,
  );
  await assert.rejects(
    readFile(join(application, "node_modules/readable-stream/package.json")),
    { code: "ENOENT" },
    "Consumers must not install a second upstream Stream implementation",
  );
  await verifyStreamSelection();
  const primitives = [
    "tcp",
    "dns",
    "tls",
    "random",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sha512-224",
    "sha512-256",
    "md5",
    "hmac-sha256",
    "pbkdf2-sha256",
    "unicode-nfkc",
  ];
  for (const name of primitives) {
    const wit = await run(process.execPath, [
      join(root, "sdk/node_modules/@bytecodealliance/jco/src/jco.js"),
      "wit",
      join(application, "node_modules/@hibana", name, "dist", `${name}.wasm`),
    ]);
    const plan = await resolveExtensions({
      root: application,
      main: "app.mjs",
      extensions: [`@hibana/${name}`],
    });
    assert.equal(plan.components.length, 1, name);
    assert.equal(plan.imports.length, 1, name);
    assert.deepEqual(plan.aliases, {}, name);
    assert.deepEqual(plan.preload, [], name);
    if (name === "tcp") {
      assert.match(wit, /import wasi:sockets\/tcp@/);
      assert.doesNotMatch(wit, /ip-name-lookup|hibana:tls/);
    } else if (name === "dns") {
      assert.match(wit, /import wasi:sockets\/ip-name-lookup@/);
      assert.doesNotMatch(wit, /import wasi:sockets\/tcp/);
    } else assert.doesNotMatch(wit, /import wasi:sockets/, name);
    if (!["random", "tls"].includes(name))
      assert.doesNotMatch(wit, /import wasi:random/, name);
    if (name.startsWith("sha") || name === "md5") {
      const printed = await run(process.execPath, [
        join(root, "sdk/node_modules/@bytecodealliance/jco/src/jco.js"),
        "print",
        plan.components[0],
      ]);
      const operations = [
        ...printed.matchAll(/\(export "hibana:[^"]+#([^"]+)"/g),
      ].map((match) => match[1]);
      assert.deepEqual(operations, ["digest"], name);
    }
  }
  const tcpPg = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: ["@hibana/postgres-tcp"],
  });
  assert.ok(!tcpPg.imports.some((name) => name.startsWith("hibana:tls/")));
  assert.equal(tcpPg.aliases.tls, undefined);
  assert.ok(!tcpPg.imports.some((name) => /sha224|sha384|sha512/.test(name)));
  const fullPg = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: ["@hibana/postgres"],
  });
  assert.ok(fullPg.imports.includes("hibana:tls/api@0.5.0"));
  const scramPg = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: ["@hibana/postgres-scram"],
  });
  assert.deepEqual(scramPg.metadata.roots, ["@hibana/postgres-scram"]);
  assert.equal(scramPg.metadata.extensions.length, 20);
  assert.equal(
    scramPg.aliases.pg,
    await realpath(
      join(application, "node_modules/@hibana/postgres-scram/dist/index.mjs"),
    ),
  );
  assert.ok(
    !scramPg.metadata.extensions.some(({ name }) =>
      /(?:md5|sha224|sha384|sha512|postgres-tcp)$/.test(name),
    ),
    "The SCRAM preset must not include unused authentication or certificate digests",
  );
  const presetMetadata = JSON.parse(
    await readFile(
      join(
        application,
        "node_modules/@hibana/postgres-scram/dist/upstream.json",
      ),
    ),
  );
  assert.deepEqual(presetMetadata.bundledPackages, []);
  await assert.rejects(
    resolveExtensions({
      root: application,
      main: "app.mjs",
      extensions: ["@hibana/postgres-scram", "@hibana/postgres"],
    }),
    /Extension alias conflict for pg/,
  );
  const corePg = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: ["@hibana/postgres-core"],
  });
  assert.deepEqual(
    corePg.components,
    [],
    "Protocol core must contain no communication or authentication Wasm",
  );
  const poolPg = await resolveExtensions({
    root: application,
    main: "app.mjs",
    extensions: ["@hibana/postgres-pool"],
  });
  assert.deepEqual(poolPg.components, []);
  assert.deepEqual(poolPg.permissions, []);
  assert.deepEqual(poolPg.aliases.pg, undefined);
  for (const name of ["postgres-core", "postgres-pool"]) {
    const metadata = JSON.parse(
      await readFile(
        join(application, "node_modules/@hibana", name, "dist/upstream.json"),
      ),
    );
    assert.equal(
      metadata.bundledPackages.includes("pg-pool"),
      name === "postgres-pool",
    );
    if (name === "postgres-pool")
      assert.deepEqual(metadata.bundledPackages, ["pg-pool"]);
  }
  for (const [options, expected] of [
    [
      {},
      [
        "tcp",
        "dns",
        "tls",
        "random",
        "sha256",
        "hmac-sha256",
        "pbkdf2-sha256",
        "unicode-nfkc",
      ],
    ],
    [{ tls: false, scram: false, md5: true }, ["tcp", "dns", "md5"]],
    [{ tls: false, scram: false, trust: true }, ["tcp", "dns"]],
    [
      { certificateHashes: ["sha384"] },
      [
        "tcp",
        "dns",
        "tls",
        "random",
        "sha256",
        "sha384",
        "hmac-sha256",
        "pbkdf2-sha256",
        "unicode-nfkc",
      ],
    ],
  ]) {
    const selection = await writePostgresSelection(application, options);
    const plan = await resolveExtensions({
      root: application,
      main: "app.mjs",
      extensions: [selection],
    });
    assert.deepEqual(
      plan.components
        .map((path) => path.split("/").at(-1).replace(".wasm", ""))
        .sort(),
      expected.sort(),
    );
    if (Object.keys(options).length === 0) {
      for (const key of ["components", "imports", "permissions", "preload"])
        assert.deepEqual(
          [...scramPg[key]].sort(),
          [...plan[key]].sort(),
          `The SCRAM preset must match the individually selected configuration: ${key}`,
        );
      assert.deepEqual(
        scramPg.metadata.extensions
          .filter(({ name }) => name !== "@hibana/postgres-scram")
          .sort((a, b) => a.name.localeCompare(b.name)),
        plan.metadata.extensions
          .filter(({ name }) => name !== selection)
          .sort((a, b) => a.name.localeCompare(b.name)),
      );
    }
  }
  console.log(
    "PASS the packaged pg SCRAM preset replaces local wiring with the same 8 Wasm components and no duplicated driver implementation",
  );
  console.log(
    "PASS PostgreSQL core, SCRAM-only, MD5-only, trust-only and explicit certificate hash selections include only their declared Wasm",
  );
  console.log(
    "PASS PostgreSQL Client core excludes pg-pool; optional Pool has no driver, transport, authentication or Wasm implementation",
  );
  console.log(
    "PASS all 14 primitive packages contain one independent component; TCP/DNS/TLS/hash imports stay separate",
  );
  console.log(
    "PASS PostgreSQL TCP-only selection excludes TLS and certificate digests despite all packages being installed",
  );
  const sha = await request(
    "sha256",
    `import {digest} from "@hibana/sha256";export default {fetch(){return Response.json({hex:Array.from(digest(new TextEncoder().encode("abc")),b=>b.toString(16).padStart(2,"0")).join(""),buffer:typeof Buffer,process:typeof process});}};`,
  );
  assert.deepEqual(sha, {
    hex: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    buffer: "undefined",
    process: "undefined",
  });
  console.log(
    "PASS SHA-256 alone serves HTTP without Node globals, DNS, TCP or TLS",
  );
  const dns = await request(
    "dns",
    `import {lookup} from "@hibana/dns";export default {async fetch(){const query=lookup("localhost"),addresses=[],deadline=Date.now()+5000;try{while(Date.now()<deadline){const answer=query.next();if(answer.address)addresses.push(answer.address);if(answer.done)return Response.json({addresses});await new Promise(done=>setTimeout(done,1));}throw new Error("DNS timeout");}finally{query.close();query[Symbol.dispose||Symbol.for("dispose")]?.();}}};`,
    { network: true },
  );
  assert.ok(
    dns.addresses.includes("127.0.0.1") || dns.addresses.includes("::1"),
  );
  console.log("PASS DNS alone resolves a hostname while TCP is disabled");
  const tls = await request(
    "tls",
    `import {createClient} from "@hibana/tls";export default {fetch(){const session=createClient({serverName:"localhost",caPem:undefined,alpn:[]});try{return Response.json({secure:session.status().secure,hello:session.takeOutput().length,certificate:session.peerCertificate()??null});}finally{session.close();session[Symbol.dispose||Symbol.for("dispose")]?.();}}};`,
  );
  assert.equal(tls.secure, false);
  assert.ok(tls.hello > 0);
  assert.equal(tls.certificate, null);
  console.log(
    "PASS standalone TLS engine generates ClientHello without TCP, DNS or Node APIs",
  );
  const streams = await request(
    "node-stream",
    await readFile(join(root, "scripts/fixtures/network/streams.mjs"), "utf8"),
    { components: 0 },
  );
  assert.deepEqual(streams, expectedStreams);
  console.log(
    "PASS selective stream entries run on Wasm without network access or the full stream entry",
  );
  assert.deepEqual(
    await request("node-stream", barrelSource, { components: 0 }),
    expectedBarrel,
  );
  console.log(
    "PASS re-exported Readable runs on Wasm with byte initialization and without optional stream APIs",
  );
  assert.deepEqual(
    await request("node-stream", writableSource, { components: 0 }),
    expectedWritable,
  );
  console.log(
    "PASS Writable alone runs on Wasm with UTF-8, backpressure and finish/close, without Readable or Duplex",
  );
} finally {
  await stop();
  await rm(folder, { recursive: true, force: true });
}
