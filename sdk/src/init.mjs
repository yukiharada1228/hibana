import { cp, mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { resolve, join, dirname, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { run } from "./process.mjs";
import { cliRelease } from "./package.mjs";

const sdk = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const templates = ["hono", "javascript", "rust", "go"];

export async function init(directory = ".", { template = "hono", install = true, cliPackage } = {}) {
  if (!templates.includes(template)) throw new Error(`Supported templates: ${templates.join(", ")}`);
  const root = resolve(directory);
  const name = basename(root).toLowerCase().replace(/[^a-z0-9-]/g, "-").replace(/^-+|-+$/g, "").slice(0, 63).replace(/-+$/, "");
  if (!/^[a-z]/.test(name)) throw new Error("Choose a project directory starting with a letter");
  await mkdir(root, { recursive: true });
  if ((await readdir(root)).some(name => name !== ".git")) throw new Error("Choose an empty project directory");
  await cp(join(sdk, "templates", template), root, { recursive: true });
  const config = { name, secrets: [], vars: template === "hono" ? {} : { GREETING: "Hello Hibana" }, limits: { memory_mb: 256, timeout_ms: 15000 } };
  const javascript = template === "hono" || template === "javascript";
  if (javascript) {
    config.main = "src/index.ts";
    const { version } = JSON.parse(await readFile(join(sdk, "package.json"), "utf8"));
    const cliDependency = cliPackage ? `file:${resolve(cliPackage)}` : cliRelease(version);
    await writeFile(join(root, "package.json"), JSON.stringify({ name, private: true, type: "module", scripts: { dev: "hibana dev", build: "hibana build", deploy: "hibana deploy" }, ...(template === "hono" ? { dependencies: { hono: "^4.6.0" } } : {}), devDependencies: { "@hibana/cli": cliDependency }, engines: { node: ">=24" } }, null, 2) + "\n");
  } else {
    // A generated project owns its WIT, and can also be built without the Hibana CLI.
    await cp(join(sdk, "wit"), join(root, "wit"), { recursive: true });
    if (template === "rust") {
      config.component = "target/wasm32-wasip2/release/hibana_http_app.wasm";
      config.build = { commands: [["cargo", "build", "--release", "--target", "wasm32-wasip2"]], watch: ["src", "Cargo.toml", "Cargo.lock", "wit"] };
    } else {
      config.component = "main.wasm";
      const tool = ["go", "tool", "componentize-go", "--ignore-toml-files", "--wit-path", "wit", "--world", "http"];
      config.build = { commands: [[...tool, "bindings", "--format", "--pkg-name", "example.com/hibana-app/bindings", "--export-pkg-name", "example.com/hibana-app", "--output", "bindings"], [...tool, "build"]], watch: ["main.go", "export_wasi_http_incoming_handler", "go.mod", "go.sum", "wit"] };
    }
  }
  await writeFile(join(root, "hibana.json"), JSON.stringify(config, null, 2) + "\n");
  await writeFile(join(root, ".gitignore"), "node_modules/\n.hibana/\n.dev.vars\ntarget/\n*.wasm\n" + (template === "go" ? "# Generated WIT bindings\nbindings/\n" : ""));
  if (javascript && install) await run("npm", ["install"], { cwd: root });
  console.log(`Created ${root}\n${javascript ? `${install ? "" : "Run npm install, then "}npm run dev` : "Run hibana dev (requires the language toolchain)"} in the project directory.`);
}
