import { findComponent } from "./api.mjs";
import { egressPolicy } from "./output.mjs";

export async function egress(
  api,
  config,
  [action, ...destinations],
  options = {},
) {
  const component = await findComponent(api, config.name);
  if (!component)
    throw new Error("Deploy this application before managing egress");
  const path = `/components/${encodeURIComponent(component.component_id || component.id)}/egress`;
  const policy = await api.request(
    path,
    action === "list"
      ? undefined
      : { method: "PATCH", body: { [action]: destinations } },
  );
  if (action === "list") {
    if (options.json) console.log(JSON.stringify(policy, null, 2));
    else egressPolicy(policy);
  } else {
    console.log(
      "Updated outbound destinations for all existing and future versions:",
    );
    console.log(
      policy.allow_outbound.length
        ? policy.allow_outbound.join("\n")
        : "None (outbound access denied)",
    );
  }
}
