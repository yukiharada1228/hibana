// Publisher-only port of a pinned pg release. There are no install hooks,
// runtime patches, or global Node builtin aliases in the distributed package.
import { build } from "esbuild";
import { createRequire } from "node:module";
import {
  copyFile,
  mkdir,
  readFile,
  readdir,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";

const root = fileURLToPath(new URL(".", import.meta.url));
const require = createRequire(import.meta.url);
const pg = dirname(require.resolve("pg/package.json"));
const version = JSON.parse(
  await readFile(join(pg, "package.json"), "utf8"),
).version;
if (version !== "8.23.0")
  throw new Error("Review the PostgreSQL adapter before upgrading pg");
await buildComponent({
  root,
  artifact: "hibana_postgres_crypto.wasm",
  output: "crypto.wasm",
});

const replacements = new Map([
  [join(pg, "lib/crypto/utils.js"), "crypto.mjs"],
  [join(pg, "lib/stream.js"), "stream.mjs"],
]);
const scoped = {
  util: "util.mjs",
  "util/types": "types.mjs",
  pgpass: "unsupported.mjs",
  fs: "unsupported.mjs",
  dns: "unsupported.mjs",
};
const result = await build({
  absWorkingDir: root,
  entryPoints: ["src/index.mjs"],
  outfile: "dist/pg.mjs",
  bundle: true,
  platform: "browser",
  format: "esm",
  target: "es2022",
  metafile: true,
  inject: [join(root, "src/immediate.mjs")],
  external: ["net", "tls", "stream", "hibana:postgres/crypto@0.1.0"],
  plugins: [
    {
      name: "pg-wasi-port",
      setup(builder) {
        builder.onLoad(
          { filter: /[/\\]pg[/\\]lib[/\\]crypto[/\\]sasl\.js$/ },
          async (args) => {
            const source = await readFile(args.path, "utf8");
            const original =
              "return password.replace(nonAsciiSpace, ' ').replace(mappedToNothing, '').normalize('NFKC')";
            if (source.split(original).length !== 2)
              throw new Error(
                "Review pg SASLprep before rebuilding the adapter",
              );
            return {
              contents: source.replace(
                original,
                "return crypto.normalizeNfkc(password.replace(nonAsciiSpace, ' ').replace(mappedToNothing, ''))",
              ),
              loader: "js",
            };
          },
        );
        builder.onResolve({ filter: /.*/ }, (args) => {
          if (args.path === "net" && args.kind === "require-call")
            return { path: join(root, "src/net.mjs") };
          if (args.path === "pg") return { path: join(root, "src/index.mjs") };
          if (scoped[args.path])
            return { path: join(root, "src", scoped[args.path]) };
          if (args.path.startsWith(".")) {
            const replaced = replacements.get(
              resolve(dirname(args.importer), args.path + ".js"),
            );
            if (replaced) return { path: join(root, "src", replaced) };
          }
        });
      },
    },
  ],
});
if (
  result.metafile.outputs["dist/pg.mjs"].imports.some(
    (entry) => entry.kind === "require-call",
  )
)
  throw new Error("The pg adapter must not retain dynamic CommonJS imports");
// Include license notices for every bundled npm package, as well as Rust's.
const packages = new Map();
for (const file of Object.keys(result.metafile.inputs)) {
  if (!file.startsWith("node_modules/")) continue;
  const parts = file.split("/");
  const name = parts[1].startsWith("@")
    ? parts.slice(1, 3).join("/")
    : parts[1];
  const directory = join(root, "node_modules", name);
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
    root,
    "dist/licenses",
    `npm-${name.replaceAll("/", "-")}-${metadata.version}`,
  );
  await mkdir(destination, { recursive: true });
  for (const entry of await readdir(directory)) {
    if (/^(license|licence|copying|notice)([._-]|$)/i.test(entry))
      await copyFile(join(directory, entry), join(destination, entry));
  }
}
await writeFile(join(root, "dist/NOTICE-JS.txt"), notices.join("\n"));
await writeFile(
  join(root, "dist/upstream.json"),
  JSON.stringify(
    { pg: version, imports: result.metafile.outputs["dist/pg.mjs"].imports },
    null,
    2,
  ) + "\n",
);
