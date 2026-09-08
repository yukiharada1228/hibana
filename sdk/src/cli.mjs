#!/usr/bin/env node
import { parseArgs } from "node:util";
import { randomUUID } from "node:crypto";
import { loadConfig } from "./config.mjs";
import { build } from "./build.mjs";
import { init, templates } from "./init.mjs";
import { apiClient, findComponent, deploy, rollback } from "./api.mjs";

const options = { from: { type: "string" }, sha256: { type: "string" }, profile: { type: "string" }, url: { type: "string" }, tenant: { type: "string" }, email: { type: "string" }, "ingress-domain": { type: "string" }, "password-stdin": { type: "boolean" }, "cli-package": { type: "string" }, source: { type: "string" }, config: { type: "string", short: "c" }, name: { type: "string" }, all: { type: "boolean" }, "all-tenants": { type: "boolean" }, yes: { type: "boolean", short: "y" }, force: { type: "boolean" }, "dry-run": { type: "boolean" }, local: { type: "boolean" }, cluster: { type: "string" }, kubeconfig: { type: "string" }, context: { type: "string" }, image: { type: "string" }, overlay: { type: "string" }, template: { type: "string" }, "no-install": { type: "boolean" }, "no-watch": { type: "boolean" }, port: { type: "string" }, runtime: { type: "string" }, version: { type: "string" }, help: { type: "boolean", short: "h" } };
const argv = process.argv.slice(2);
const versionOnly = argv.length === 1 && ["--version", "-v"].includes(argv[0]);
const { values, positionals } = parseArgs({ args: versionOnly ? [] : argv, options, allowPositionals: true });
const [command, ...args] = positionals;

async function main() {
  if (versionOnly) { const { packageInfo } = await import("./package.mjs"); console.log(`hibana ${(await packageInfo()).version}`); return; }
  if (command === "runtime") {
    if (values.help || args.length !== 1 || args[0] !== "install") {
      console.log("hibana runtime install [--version VERSION] [--from FILE --sha256 HASH]");
      if (!values.help && args.length) process.exitCode = 1;
      return;
    }
    const { installRuntime } = await import("./runtime.mjs"); return installRuntime(values);
  }
  if (command === "platform") { const { platform } = await import("./platform.mjs"); return platform(args, values); }
  if (values.help || !command) { console.log("hibana init [directory] --template hono|javascript|rust|go\nhibana build [-c hibana.json]\nhibana runtime install [--version VERSION]\nhibana dev [--port 8787] [--runtime /path/to/hibana-worker] [--no-watch]\nhibana login --profile NAME --url https://API_HOST --tenant TEAM --email EMAIL --password-stdin\nhibana logout [--profile NAME]\nhibana profile list|use NAME|remove NAME\nhibana deploy [--version 1.0.0]\nhibana rollback [--version 1.0.0]\nhibana list [--all-tenants]\nhibana delete [NAME] [--name NAME] [-c hibana.json] [--dry-run] [--yes]\nhibana delete --all [--all-tenants] --yes\nhibana platform install|start|stop|uninstall|status|test [--help]\nhibana secret put NAME < value.txt\nhibana secret list\nhibana secret delete NAME\nhibana secret allow-deploy NAME\nhibana secret deny-deploy NAME\n\nRemote commands: --profile NAME / HIBANA_PROFILE; --url overrides the endpoint.\nHIBANA_URL applies when --profile is not explicitly selected.\nAuthentication: HIBANA_TOKEN or hibana login. No implicit local server.\ninit --cli-package PATH installs a local CLI tarball explicitly."); return; }
  if (command === "init") {
    if (args.length > 1) throw new Error(`Usage: hibana init [directory] --template ${templates.join("|")}`);
    return init(args[0], { template: values.template, install: !values["no-install"], cliPackage: values["cli-package"] });
  }
  if (command === "profile") { const { profileCommand } = await import("./profiles.mjs"); return profileCommand(args); }
  if (command === "logout") { const { logout } = await import("./profiles.mjs"); return logout(values); }
  if (command === "login") {
    if (args.length) throw new Error("Usage: hibana login --profile NAME --url URL --tenant TEAM --email EMAIL --password-stdin");
    let password = process.env.HIBANA_PASSWORD;
    if (values["password-stdin"]) {
      if (process.stdin.isTTY) throw new Error("Pipe the password through stdin with --password-stdin");
      const chunks = []; let size = 0;
      for await (const chunk of process.stdin) { size += chunk.length; if (size > 4096) throw new Error("Password exceeds 4096 bytes"); chunks.push(chunk); }
      password = Buffer.concat(chunks).toString("utf8").replace(/\r?\n$/, "");
    }
    const api = await apiClient(process.cwd(), { ...values, login: true });
    const name = await api.login({ password, persist: "profile" });
    console.log(`Logged in to ${api.url}. Selected profile: ${name}`);
    return;
  }
  if (command === "delete") { const { deleteApplication } = await import("./delete.mjs"); return deleteApplication(args, values); }
  if (command === "list") { const { deletionClient } = await import("./delete.mjs"); const api = await deletionClient(values); console.log(JSON.stringify(await api.request(values["all-tenants"] ? "/admin/components" : "/components"), null, 2)); return; }
  const config = await loadConfig(values.config);
  if (command === "build") { console.log(await build(config)); return; }
  if (command === "dev") { const { dev } = await import("./dev.mjs"); await dev(config, values, build); return; }
  const api = await apiClient(config.root, values);
  if (command === "deploy") {
    const version = values.version || `0.0.0-dev.${Date.now()}.${randomUUID().slice(0, 8)}`;
    const result = await deploy(api, config, await build(config), version);
    console.log(`Deployed ${config.name} (${result.component_id}) version ${result.version}`);
    console.log(`Server: ${api.url}`);
    if (api.ingressDomain && api.tenant) console.log(`HTTP host: ${config.name}.${api.tenant}.${api.ingressDomain}`);
    return;
  }
  if (command === "rollback") {
    if (args.length) throw new Error("Usage: hibana rollback [--version VERSION]");
    const result = await rollback(api, config, values.version);
    console.log(`Rolled back ${config.name} to ${result.active_version_id}. Code and vars were restored. Secret values and external data are unchanged.`);
    return;
  }
  if (command === "secret") {
    const [action, name] = args;
    if (!["list", "put", "delete", "allow-deploy", "deny-deploy"].includes(action) || (action !== "list" && !/^[A-Z_][A-Z0-9_]{0,63}$/.test(name || ""))) throw new Error("Usage: hibana secret [list | put NAME | delete NAME | allow-deploy NAME | deny-deploy NAME]");
    const component = await findComponent(api, config.name);
    if (!component) throw new Error("Deploy this application before managing secrets");
    const base = `/components/${encodeURIComponent(component.component_id || component.id)}/secrets`;
    if (action === "list") console.log(JSON.stringify(await api.request(base), null, 2));
    else if (action === "allow-deploy" || action === "deny-deploy") {
      const allowed = action === "allow-deploy";
      await api.request(`${base}/${name}/deploy-access`, { method: "PUT", body: { allowed } });
      console.log(`${allowed ? "Allowed" : "Denied"} ${name} for future deployments. Existing version bindings are unchanged.`);
    }
    else if (action === "delete") { await api.request(`${base}/${name}`, { method: "DELETE" }); console.log(`Deleted secret ${name}`); }
    else {
      if (process.stdin.isTTY) throw new Error("Pipe the secret value through stdin; it is not accepted as a command argument");
      const chunks = []; let size = 0;
      for await (const chunk of process.stdin) { size += chunk.length; if (size > 4096) throw new Error("Secret exceeds 4096 bytes"); chunks.push(chunk); }
      const value = Buffer.concat(chunks).toString("utf8").replace(/\r?\n$/, "");
      await api.request(`${base}/${name}`, { method: "PUT", body: { value } });
      console.log(`Stored secret ${name}. Add its name to hibana.json secrets, authorize it with hibana secret allow-deploy, then deploy.`);
    }
    return;
  }
  throw new Error(`Unknown command: ${command}`);
}
main().catch(error => { console.error(`hibana: ${error.message}`); process.exitCode = 1; });
