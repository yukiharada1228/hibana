#!/usr/bin/env node
import { randomUUID } from "node:crypto";
import { loadConfig } from "./config.mjs";
import { build } from "./build.mjs";
import { init } from "./init.mjs";
import { readStdin } from "./process.mjs";
import { parseCommand } from "./commands.mjs";
import { validateVersionName } from "./version-name.mjs";
import { applications, secrets, text, versionLabel } from "./output.mjs";
import {
  apiClient,
  inventoryClient,
  findComponent,
  deploy,
  rollback,
} from "./api.mjs";

async function main() {
  const { versionOnly, helpText, command, args, values } = parseCommand(
    process.argv.slice(2),
  );
  if (versionOnly) {
    const { packageInfo } = await import("./package.mjs");
    console.log(`hibana ${(await packageInfo()).version}`);
    return;
  }
  if (helpText) {
    console.log(helpText);
    return;
  }
  if (command === "runtime") {
    const { installRuntime } = await import("./runtime.mjs");
    return installRuntime(values);
  }
  if (command === "platform") {
    const { platform } = await import("./platform.mjs");
    return platform(args, values);
  }
  if (command === "init")
    return init(args[0], {
      template: values.template,
      install: !values["no-install"],
      cliPackage: values["cli-package"],
    });
  if (command === "profile") {
    const { profileCommand } = await import("./profiles.mjs");
    return profileCommand(args);
  }
  if (command === "logout") {
    const { logout } = await import("./profiles.mjs");
    return logout(values);
  }
  if (command === "login") {
    const api = await apiClient({ ...values, login: true });
    const name = await api.loginOidc({ noBrowser: values["no-browser"] });
    console.log(
      `Connected to ${api.url}\nTenant: ${text(api.tenant)}\nSaved connection: ${text(name)} (used by subsequent commands)`,
    );
    return;
  }
  if (command === "delete") {
    const { deleteApplication } = await import("./delete.mjs");
    return deleteApplication(args, values);
  }
  if (command === "list") {
    const api = await inventoryClient(values);
    const result = await api.request(
      values["all-tenants"] ? "/admin/components" : "/components",
    );
    if (values.json) console.log(JSON.stringify(result, null, 2));
    else applications(result, values["all-tenants"]);
    return;
  }
  const config = await loadConfig(values.config);
  if (command === "build") {
    console.log(
      await build(config, {
        frozenLockfile: Boolean(values["frozen-lockfile"]),
      }),
    );
    return;
  }
  if (command === "dev") {
    const { dev } = await import("./dev.mjs");
    await dev(config, values, build);
    return;
  }
  const api = await apiClient(values);
  if (command === "egress") {
    const { egress } = await import("./egress.mjs");
    return egress(api, config, args, values);
  }
  if (command === "deploy") {
    const version = validateVersionName(
      values.version ?? `0.0.0-dev.${Date.now()}.${randomUUID().slice(0, 8)}`,
    );
    const session = await api.request("/auth/session");
    if (!["read", "deploy"].every((scope) => session.scopes?.includes(scope)))
      throw new Error(
        "Read and Deploy permissions are required. Sign in with an account authorized to deploy.",
      );
    console.log(
      `Target: ${api.url}\nTenant: ${text(session.tenant_slug)}\nApp: ${config.name}`,
    );
    console.log("Building...");
    const artifact = await build(config, {
      frozenLockfile: Boolean(values["frozen-lockfile"]),
    });
    console.log("Uploading and activating...");
    const result = await deploy(api, config, artifact, version);
    console.log(`Deployed ${config.name} · ${versionLabel(result.version)}`);
    if (result.public_url) console.log(`URL: ${result.public_url}`);
    if (values.verbose)
      console.log(
        `Component ID: ${text(result.component_id)}\nVersion: ${text(result.version)}`,
      );
    return;
  }
  if (command === "rollback") {
    const result = await rollback(api, config, values.version);
    console.log(
      `Rolled back ${config.name} to ${result.version ? versionLabel(result.version) : values.version ? versionLabel(values.version) : "the previous version"}.`,
    );
    console.log(
      `Server: ${api.url}\nSecret values and external data are unchanged.`,
    );
    if (values.verbose)
      console.log(
        `Version ID: ${text(result.active_version_id)}\nVersion: ${text(result.version || values.version)}`,
      );
    return;
  }
  if (command === "secret") {
    const [action, name] = args;
    const component = await findComponent(api, config.name);
    if (!component)
      throw new Error("Deploy this application before managing secrets");
    const base = `/components/${encodeURIComponent(component.component_id)}/secrets`;
    if (action === "list") {
      const result = await api.request(base);
      if (values.json) console.log(JSON.stringify(result, null, 2));
      else secrets(result);
    } else if (action === "allow-deploy" || action === "deny-deploy") {
      const allowed = action === "allow-deploy";
      await api.request(`${base}/${name}/deploy-access`, {
        method: "PUT",
        body: { allowed },
      });
      console.log(
        `${allowed ? "Allowed" : "Denied"} ${name} for future deployments. Existing version bindings are unchanged.`,
      );
    } else if (action === "delete") {
      await api.request(`${base}/${name}`, { method: "DELETE" });
      console.log(`Deleted secret ${name}`);
    } else {
      const value = await readStdin("Secret");
      await api.request(`${base}/${name}`, { method: "PUT", body: { value } });
      console.log(
        `Stored secret ${name}. Add its name to hibana.json secrets, authorize it with hibana secret allow-deploy, then deploy.`,
      );
    }
    return;
  }
  throw new Error(`Unknown command: ${command}`);
}
main().catch((error) => {
  console.error(`hibana: ${error.hint || error.message}`);
  process.exitCode = 1;
});
