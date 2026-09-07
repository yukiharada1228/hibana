#!/usr/bin/env node
import { parseArgs } from "node:util";
import { randomUUID } from "node:crypto";
import { loadConfig } from "./config.mjs";
import { build } from "./build.mjs";
import { init, templates } from "./init.mjs";
import { apiClient, findComponent, deploy, rollback } from "./api.mjs";

const options = { config: { type: "string", short: "c" }, template: { type: "string" }, "no-install": { type: "boolean" }, "no-watch": { type: "boolean" }, port: { type: "string" }, runtime: { type: "string" }, version: { type: "string" }, help: { type: "boolean", short: "h" } };
const { values, positionals } = parseArgs({ options, allowPositionals: true });
const [command, ...args] = positionals;

async function main() {
  if (values.help || !command) { console.log("hibana init [directory] --template hono|javascript|rust|go\nhibana build [-c hibana.json]\nhibana dev [--port 8787] [--runtime /path/to/faas-worker] [--no-watch]\nhibana login\nhibana deploy [--version 1.0.0]\nhibana rollback [--version 1.0.0]\nhibana secret put NAME < value.txt\nhibana secret list\nhibana secret delete NAME\n\nServer: HIBANA_URL; authentication: HIBANA_TOKEN or hibana login."); return; }
  if (command === "init") {
    if (args.length > 1) throw new Error(`Usage: hibana init [directory] --template ${templates.join("|")}`);
    return init(args[0], { template: values.template, install: !values["no-install"] });
  }
  if (command === "login") { await (await apiClient(process.cwd())).login(); console.log("Logged in. Credentials saved in .hibana/auth.json"); return; }
  const config = await loadConfig(values.config);
  if (command === "build") { console.log(await build(config)); return; }
  if (command === "dev") { const { dev } = await import("./dev.mjs"); await dev(config, values, build); return; }
  const api = await apiClient(config.root);
  if (command === "deploy") {
    const version = values.version || `0.0.0-dev.${Date.now()}.${randomUUID().slice(0, 8)}`;
    const result = await deploy(api, config, await build(config), version);
    console.log(`Deployed ${config.name} (${result.component_id}) version ${result.version}`);
    if (process.env.HIBANA_INGRESS_DOMAIN && process.env.HIBANA_TENANT) console.log(`HTTP host: ${config.name}.${process.env.HIBANA_TENANT}.${process.env.HIBANA_INGRESS_DOMAIN}`);
    return;
  }
  if (command === "rollback") {
    if (args.length) throw new Error("Usage: hibana rollback [--version VERSION]");
    const result = await rollback(api, config, values.version);
    console.log(`Rolled back ${config.name} to ${result.active_version_id}. Environment variables and secrets are unchanged.`);
    return;
  }
  if (command === "secret") {
    const [action, name] = args;
    if (!["list", "put", "delete"].includes(action) || (action !== "list" && !/^[A-Za-z_][A-Za-z0-9_]*$/.test(name || ""))) throw new Error("Usage: hibana secret [list | put NAME | delete NAME]");
    const component = await findComponent(api, config.name);
    if (!component) throw new Error("Deploy this application before managing secrets");
    const base = `/components/${encodeURIComponent(component.component_id || component.id)}/secrets`;
    if (action === "list") console.log(JSON.stringify(await api.request(base), null, 2));
    else if (action === "delete") { await api.request(`${base}/${name}`, { method: "DELETE" }); console.log(`Deleted secret ${name}`); }
    else {
      if (process.stdin.isTTY) throw new Error("Pipe the secret value through stdin; it is not accepted as a command argument");
      const chunks = []; let size = 0;
      for await (const chunk of process.stdin) { size += chunk.length; if (size > 4096) throw new Error("Secret exceeds 4096 bytes"); chunks.push(chunk); }
      const value = Buffer.concat(chunks).toString("utf8").replace(/\r?\n$/, "");
      await api.request(`${base}/${name}`, { method: "PUT", body: { value } });
      console.log(`Stored secret ${name}. Run hibana deploy to grant it to the new version.`);
    }
    return;
  }
  throw new Error(`Unknown command: ${command}`);
}
main().catch(error => { console.error(`hibana: ${error.message}`); process.exitCode = 1; });
