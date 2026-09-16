// Publisher-only port of a pinned pg release. There are no install hooks,
// runtime patches, or global Node builtin aliases in the distributed package.
import { build } from "esbuild";
import { createRequire } from "node:module";
import {
  copyFile,
  mkdir,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const workspace = fileURLToPath(new URL(".", import.meta.url));
function replaceOnce(source, old, value) {
  if (source.split(old).length !== 2)
    throw new Error("Review pinned pg port: " + old);
  return source.replace(old, value);
}
function factory(source, parameters, removed) {
  for (const line of removed) source = replaceOnce(source, line, "");
  source = replaceOnce(source, "module.exports = ", "return ");
  return `module.exports = function(${parameters}) {\n${source}\n}`;
}
function removeSection(source, start, end) {
  if (
    source.split(start).length !== 2 ||
    source.split(end).length !== 2 ||
    source.indexOf(start) >= source.indexOf(end)
  )
    throw new Error("Review pinned pg port section: " + start);
  return (
    source.slice(0, source.indexOf(start)) + source.slice(source.indexOf(end))
  );
}
export async function buildPostgres(name) {
  if (
    ![
      "postgres-core",
      "postgres-pool",
      "postgres-auth-scram",
      "postgres",
      "postgres-tcp",
    ].includes(name)
  )
    throw new Error("Unknown PostgreSQL build target");
  const root = join(workspace, name);
  const outputRoot = root;
  const core = join(workspace, "postgres-core");
  const require = createRequire(join(core, "package.json"));
  const pg = dirname(require.resolve("pg/package.json"));
  const version = JSON.parse(
    await readFile(join(pg, "package.json"), "utf8"),
  ).version;
  if (version !== "8.23.0")
    throw new Error("Review the PostgreSQL adapter before upgrading pg");
  await rm(join(root, "dist"), { recursive: true, force: true });
  const scoped = {
    events: "events.mjs",
    util: "util.mjs",
    "util/types": "types.mjs",
    fs: "unsupported.mjs",
    dns: "unsupported.mjs",
  };
  const result = await build({
    absWorkingDir: root,
    entryPoints: ["src/index.mjs"],
    outfile: join(outputRoot, "dist/index.mjs"),
    bundle: true,
    platform: "browser",
    format: "esm",
    target: "es2022",
    metafile: true,
    inject: name === "postgres-pool" ? [join(root, "src/immediate.mjs")] : [],
    external: ["node:buffer", "node:events", "@hibana/*"],
    plugins: [
      {
        name: "pg-wasi-port",
        setup(builder) {
          builder.onLoad(
            { filter: /[/\\]pg-pool[/\\]index\.js$/ },
            async (args) => {
              if (name !== "postgres-pool")
                throw new Error(
                  "pg-pool must only be bundled in postgres-pool",
                );
              const metadata = JSON.parse(
                await readFile(
                  join(dirname(args.path), "package.json"),
                  "utf8",
                ),
              );
              if (metadata.version !== "3.14.0")
                throw new Error(
                  "Review the Pool adapter before upgrading pg-pool",
                );
              return {
                contents: replaceOnce(
                  await readFile(args.path, "utf8"),
                  "this.Client = this.options.Client || Client || require('pg').Client",
                  "this.Client = Client",
                ),
                loader: "js",
              };
            },
          );
          builder.onLoad(
            {
              filter:
                /[/\\]pg[/\\]lib[/\\](?:client|connection|crypto[/\\]sasl)\.js$/,
            },
            async (args) => {
              let source = await readFile(args.path, "utf8");
              if (args.path === join(pg, "lib/client.js")) {
                // Our authentication state machine owns these handlers. Do not
                // ship pg's second implementation (including pgpass fallback).
                source = removeSection(
                  source,
                  "  _getPassword(cb) {",
                  "  _handleBackendKeyData(msg) {",
                );
                source = removeSection(
                  source,
                  "const pgPassDeprecationNotice =",
                  "const byoPromiseDeprecationNotice =",
                );
                source = replaceOnce(
                  source,
                  "sasl.DEFAULT_MAX_SCRAM_ITERATIONS",
                  "defaultMaxScramIterations",
                );
                source = factory(
                  source,
                  "{ Connection, defaultMaxScramIterations }",
                  [
                    "const Connection = require('./connection')",
                    "const crypto = require('./crypto/utils')",
                    "const sasl = require('./crypto/sasl')",
                  ],
                );
              } else if (args.path === join(pg, "lib/connection.js")) {
                // Start transport I/O through the adapter only after pg has
                // installed all lifecycle listeners, including Client's ones.
                source = replaceOnce(
                  source,
                  "    this.stream.setNoDelay(true)\n    this.stream.connect(port, host)",
                  "    this._connectStream(port, host)",
                );
                source = replaceOnce(
                  source,
                  "const net = require('net')",
                  "const net = stream",
                );
                source = factory(source, "stream", [
                  "const stream = require('./stream')",
                ]);
              } else {
                source = replaceOnce(
                  source,
                  "return password.replace(nonAsciiSpace, ' ').replace(mappedToNothing, '').normalize('NFKC')",
                  "return crypto.normalizeNfkc(password.replace(nonAsciiSpace, ' ').replace(mappedToNothing, ''))",
                );
                source = factory(source, "crypto", [
                  "const crypto = require('./utils')",
                ]);
              }
              return { contents: source, loader: "js" };
            },
          );
          builder.onResolve({ filter: /.*/ }, (args) => {
            if (scoped[args.path])
              return { path: join(core, "src", scoped[args.path]) };
            // Any accidental return of upstream global crypto/transport dependencies
            // fails here instead of silently pulling the complete adapter back in.
            if (["crypto", "net", "tls", "pg", "pgpass"].includes(args.path))
              throw new Error(`Unexpected dependency in ${name}: ${args.path}`);
          });
        },
      },
    ],
  });
  const built = Object.values(result.metafile.outputs)[0];
  if (built.imports.some((entry) => entry.kind === "require-call"))
    throw new Error("The pg adapter must not retain dynamic CommonJS imports");
  // Include notices for every bundled npm package, including workspace-hoisted inputs.
  const packages = new Map();
  for (const file of Object.keys(result.metafile.inputs)) {
    const absolute = resolve(root, file);
    const marker = absolute.lastIndexOf("/node_modules/");
    if (marker < 0) continue;
    const parts = absolute.slice(marker + "/node_modules/".length).split("/");
    const name = parts[0].startsWith("@")
      ? parts.slice(0, 2).join("/")
      : parts[0];
    const directory = join(absolute.slice(0, marker), "node_modules", name);
    const metadata = JSON.parse(
      await readFile(join(directory, "package.json"), "utf8"),
    );
    packages.set(name, { directory, metadata });
  }
  const notices = ["Bundled JavaScript dependencies:", ""];
  for (const [name, { directory, metadata }] of [...packages].sort()) {
    notices.push(
      `${name} ${metadata.version}: ${metadata.license}`,
      `https://registry.npmjs.org/${name}/-/${name.split("/").at(-1)}-${metadata.version}.tgz`,
      "",
    );
    const destination = join(
      outputRoot,
      "dist/licenses",
      `npm-${name.replaceAll("/", "-")}-${metadata.version}`,
    );
    await mkdir(destination, { recursive: true });
    for (const entry of await readdir(directory)) {
      if (/^(license|licence|copying|notice)([._-]|$)/i.test(entry))
        await copyFile(join(directory, entry), join(destination, entry));
    }
  }
  await writeFile(join(outputRoot, "dist/NOTICE-JS.txt"), notices.join("\n"));
  await writeFile(
    join(outputRoot, "dist/upstream.json"),
    JSON.stringify(
      {
        pg: version,
        package: name,
        bundledPackages: [...packages.keys()].sort(),
        imports: built.imports,
      },
      null,
      2,
    ) + "\n",
  );
}
