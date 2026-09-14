#!/usr/bin/env node
import { randomUUID } from "node:crypto";
import { loadConfig } from "./config.mjs";
import { build } from "./build.mjs";
import { init } from "./init.mjs";
import { parseCommand } from "./commands.mjs";
import { apiClient, findComponent, deploy, rollback } from "./api.mjs";

async function main() {
  const { versionOnly, helpText, command, args, values } = parseCommand(process.argv.slice(2));
  if (versionOnly) { const { packageInfo } = await import("./package.mjs"); console.log(`hibana ${(await packageInfo()).version}`); return; }
  if (helpText) { console.log(helpText); return; }
  if (command === "runtime") { const { installRuntime } = await import("./runtime.mjs"); return installRuntime(values); }
  if (command === "platform") { const { platform } = await import("./platform.mjs"); return platform(args, values); }
  if (command === "init") return init(args[0], { template: values.template, install: !values["no-install"], cliPackage: values["cli-package"] });
  if (command === "profile") { const { profileCommand } = await import("./profiles.mjs"); return profileCommand(args); }
  if (command === "logout") { const { logout } = await import("./profiles.mjs"); return logout(values); }
  if (command === "login") {
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
    const result = await rollback(api, config, values.version);
    console.log(`Rolled back ${config.name} to ${result.active_version_id}. Code and vars were restored. Secret values and external data are unchanged.`);
    return;
  }
  if (command === "secret") {
    const [action, name] = args;
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
