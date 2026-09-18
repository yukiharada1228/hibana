import { isApplicationName } from "./application-name.mjs";
import { resolve } from "node:path";
import { inventoryClient, findComponent } from "./api.mjs";
import { confirm } from "./confirm.mjs";
import { readConfigFile } from "./config.mjs";

export async function deleteApplication(args, options) {
  let [name] = args;
  if (options.all && name) throw new Error("Choose a name or --all, not both");
  if (options["all-tenants"] && !options.all)
    throw new Error("--all-tenants requires --all");
  const path = resolve(options.config || "hibana.json");
  if (!name && !options.all) name = (await readConfigFile(path)).name;
  if (!options.all && !isApplicationName(name))
    throw new Error("Application name must be a lowercase DNS label");
  // Deletion never builds or reads application sources. Dry-run also requires no credentials.
  if (options["dry-run"] && !options.all) {
    console.log(`Would delete application ${name}. No changes made.`);
    return;
  }
  const api = await inventoryClient(options);
  const inventory = options.all
    ? await api.request(
        options["all-tenants"] ? "/admin/components" : "/components",
      )
    : [await findComponent(api, name)].filter(Boolean);
  const components = Array.isArray(inventory)
    ? inventory
    : inventory.components;
  if (!components.length) {
    console.log("No matching applications are deployed.");
    return;
  }
  for (const component of components)
    console.log(
      `${component.tenant_slug ? component.tenant_slug + "/" : ""}${component.name}`,
    );
  if (options["dry-run"]) {
    console.log(
      `Would delete ${components.length} application(s). No changes made.`,
    );
    return;
  }
  if (
    !(await confirm(
      `Delete ${components.length} application(s) from ${api.url}?`,
      options.yes,
    ))
  ) {
    console.log("Cancelled.");
    return;
  }
  for (const component of components) {
    const base = options["all-tenants"]
      ? `/admin/tenants/${encodeURIComponent(component.tenant_id)}/components`
      : "/components";
    await api.request(
      `${base}/${encodeURIComponent(component.component_id || component.id)}`,
      { method: "DELETE" },
    );
    console.log(
      `Deleted application ${component.tenant_slug ? component.tenant_slug + "/" : ""}${component.name}.`,
    );
  }
}
