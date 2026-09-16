// Publisher-only port of readable-stream. Consumers use the generated modules;
// neither node_modules nor application code is patched during installation.
import { build } from "esbuild";
import { createRequire } from "node:module";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const upstream = dirname(require.resolve("readable-stream/package.json"));
const metadata = JSON.parse(
  await readFile(join(upstream, "package.json"), "utf8"),
);
if (metadata.version !== "4.7.0")
  throw new Error("Review the Stream port before upgrading readable-stream");

function replaceOnce(source, before, after) {
  if (source.split(before).length !== 2)
    throw new Error(`Review pinned Stream port: ${before}`);
  return source.replace(before, after);
}

// Keep one shared copy of the modules reachable from the complete browser API.
// Per-feature application imports then select their own subset of this graph.
const graph = await build({
  absWorkingDir: upstream,
  entryPoints: ["lib/ours/browser.js"],
  bundle: true,
  platform: "browser",
  format: "esm",
  external: Object.keys(metadata.dependencies),
  metafile: true,
  write: false,
});
const output = join(root, "dist/readable-stream");
await rm(join(root, "dist"), { recursive: true, force: true });
for (const input of Object.keys(graph.metafile.inputs)) {
  const path = relative(upstream, resolve(upstream, input));
  if (!path.startsWith("lib/") || !path.endsWith(".js"))
    throw new Error(`Unexpected Stream build input: ${input}`);
  let source = await readFile(join(upstream, path), "utf8");
  if (/\/streams\/(readable|writable)\.js$/.test(path)) {
    source = replaceOnce(
      source,
      "const process = require('process/')",
      "const process = require('process/')\nconst duplexType = require('./duplex-type')",
    );
    for (const value of ["stream", "this"])
      source = replaceOnce(
        source,
        `${value} instanceof require('./duplex')`,
        `duplexType.isDuplex(${value})`,
      );
  } else if (path === "lib/internal/streams/duplex.js") {
    source = replaceOnce(
      source,
      "module.exports = Duplex",
      "module.exports = Duplex\nrequire('./duplex-type').setDuplex(Duplex)",
    );
  }
  const destination = join(output, path);
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, source);
}

// Only Duplex supplies this constructor. The one-way reference lets Readable
// and Writable retain upstream instanceof semantics without importing Duplex.
await writeFile(
  join(output, "lib/internal/streams/duplex-type.js"),
  `let Duplex;
exports.setDuplex = (constructor) => { Duplex = constructor; };
exports.isDuplex = (value) => Duplex !== undefined && value instanceof Duplex;
`,
);
await writeFile(
  join(output, "package.json"),
  JSON.stringify({ type: "commonjs" }, null, 2) + "\n",
);
await copyFile(join(upstream, "LICENSE"), join(output, "LICENSE"));
await writeFile(
  join(root, "dist/NOTICE.txt"),
  "readable-stream 4.7.0 (MIT)\nhttps://github.com/nodejs/readable-stream\n" +
    "Readable/Writable use a shared Duplex type reference instead of importing Duplex.\n" +
    "All public entries use the same generated modules. See readable-stream/LICENSE.\n",
);
