import { readFile } from "node:fs/promises";
import { resolve, dirname, isAbsolute } from "node:path";
export async function loadConfig(file = "hibana.json") {
  const path = resolve(file);
  const value = JSON.parse(await readFile(path, "utf8"));
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("hibana.json must be an object");
  const supported = new Set(["name", "main", "component", "build", "vars", "limits"]);
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
  if (!vars || Array.isArray(vars) || typeof vars !== "object" || Object.entries(vars).some(([k,v]) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(k) || typeof v !== "string")) throw new Error("vars must contain string environment values");
  if (value.limits !== undefined && (!value.limits || typeof value.limits !== "object" || Array.isArray(value.limits))) throw new Error("limits must be an object");
  const limits = { memory_mb: 256, timeout_ms: 15000, ...value.limits };
  for (const key of Object.keys(limits)) if (!["memory_mb", "timeout_ms", "fuel"].includes(key)) throw new Error(`Unsupported limit: ${key}`);
  if (!Number.isSafeInteger(limits.memory_mb) || limits.memory_mb < 1 || limits.memory_mb > 1024) throw new Error("memory_mb must be 1..1024");
  if (!Number.isSafeInteger(limits.timeout_ms) || limits.timeout_ms < 1 || limits.timeout_ms > 30000) throw new Error("timeout_ms must be 1..30000");
  if (limits.fuel !== undefined && (!Number.isSafeInteger(limits.fuel) || limits.fuel < 1)) throw new Error("fuel must be a positive safe integer");
  return { ...value, vars, root: dirname(path), path, resources: { max_memory_bytes: limits.memory_mb * 1024 * 1024, max_wall_time_ms: limits.timeout_ms, max_execution_time_ms: limits.timeout_ms + 5000, ...(limits.fuel === undefined ? {} : { max_fuel: limits.fuel }) } };
}
