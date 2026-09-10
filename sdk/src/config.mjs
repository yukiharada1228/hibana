import { readFile } from "node:fs/promises";
import { resolve, dirname, isAbsolute } from "node:path";
export async function readConfigFile(file = "hibana.json") {
  const path = resolve(file);
  let contents, value;
  try { contents = await readFile(path, "utf8"); }
  catch (error) {
    if (error.code === "ENOENT") throw new Error(`Project configuration not found: ${path}\nRun from your application directory, use --config FILE, or create an app with hibana init my-api.`, { cause: error });
    throw error;
  }
  try { value = JSON.parse(contents); }
  catch (error) { throw new Error(`Invalid JSON in ${path}. Check the file's JSON syntax.`, { cause: error }); }
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("hibana.json must be an object");
  return value;
}
export async function loadConfig(file = "hibana.json") {
  const path = resolve(file);
  const value = await readConfigFile(path);
  const supported = new Set(["name", "main", "component", "build", "vars", "secrets", "limits"]);
  for (const key of Object.keys(value)) if (!supported.has(key)) throw new Error(`Unsupported hibana.json field: ${key}`);
  if (!/^[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(value.name || "")) throw new Error("name must be a lowercase DNS label (1..63 characters)");
  if ((value.main !== undefined) === (value.component !== undefined)) throw new Error("Specify exactly one of main (JavaScript/TypeScript) or component (.wasm)");
  const input = value.main ?? value.component;
  if (typeof input !== "string" || !input.trim() || input.includes("\0")) throw new Error("main/component must be a non-empty path");
  if (value.build !== undefined) {
    const build = value.build;
    if (!value.component || !build || typeof build !== "object" || Array.isArray(build)) throw new Error("build requires a component path and an object");
    for (const key of Object.keys(build)) if (!["commands", "watch"].includes(key)) throw new Error(`Unsupported build field: ${key}`);
    if (!Array.isArray(build.commands) || !build.commands.length || build.commands.some(args => !Array.isArray(args) || !args.length || args.some(arg => typeof arg !== "string" || arg.includes("\0")) || !args[0].trim())) throw new Error("build.commands must be a non-empty list of command argument arrays");
    if (build.watch !== undefined && (!Array.isArray(build.watch) || !build.watch.length || build.watch.some(path => typeof path !== "string" || !path.trim() || isAbsolute(path) || path.includes("\0") || path.split(/[\\/]/).some(part => part === "..")))) throw new Error("build.watch must list project-relative files or directories (no globs or parent paths)");
    if (build.watch?.some(path => /[*?\[\]{}]/.test(path))) throw new Error("build.watch paths do not support globs");
  }
  const vars = value.vars === undefined ? {} : value.vars;
  if (!vars || Array.isArray(vars) || typeof vars !== "object" || Object.entries(vars).some(([k,v]) => !/^[A-Z_][A-Z0-9_]{0,63}$/.test(k) || typeof v !== "string")) throw new Error("vars must contain string environment values");
  const secrets = value.secrets === undefined ? [] : value.secrets;
  if (!Array.isArray(secrets) || secrets.some(name => typeof name !== "string" || !/^[A-Z_][A-Z0-9_]{0,63}$/.test(name))) throw new Error("secrets must list environment names (values belong in hibana secret put)");
  if (new Set(secrets).size !== secrets.length) throw new Error("secrets must not contain duplicate names");
  if (secrets.some(name => Object.hasOwn(vars, name))) throw new Error("vars and secrets must use distinct names");
  if (Object.keys(vars).length + secrets.length > 64) throw new Error("At most 64 vars and secrets are allowed");
  if (Object.values(vars).some(value => Buffer.byteLength(value) > 4096)) throw new Error("Each var must be at most 4096 bytes");
  if (Object.entries(vars).reduce((n, [key, value]) => n + Buffer.byteLength(key) + Buffer.byteLength(value), 0) > 32768) throw new Error("vars must total at most 32768 bytes");
  if (value.limits !== undefined && (!value.limits || typeof value.limits !== "object" || Array.isArray(value.limits))) throw new Error("limits must be an object");
  const limits = { memory_mb: 256, timeout_ms: 15000, ...value.limits };
  for (const key of Object.keys(limits)) if (!["memory_mb", "timeout_ms", "fuel"].includes(key)) throw new Error(`Unsupported limit: ${key}`);
  if (!Number.isSafeInteger(limits.memory_mb) || limits.memory_mb < 1 || limits.memory_mb > 1024) throw new Error("memory_mb must be 1..1024");
  if (!Number.isSafeInteger(limits.timeout_ms) || limits.timeout_ms < 1 || limits.timeout_ms > 30000) throw new Error("timeout_ms must be 1..30000");
  if (limits.fuel !== undefined && (!Number.isSafeInteger(limits.fuel) || limits.fuel < 1)) throw new Error("fuel must be a positive safe integer");
  return { ...value, vars, secrets, root: dirname(path), path, resources: { max_memory_bytes: limits.memory_mb * 1024 * 1024, max_wall_time_ms: limits.timeout_ms, max_execution_time_ms: limits.timeout_ms + 5000, ...(limits.fuel === undefined ? {} : { max_fuel: limits.fuel }) } };
}
