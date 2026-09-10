import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { run } from "./process.mjs";
import { confirm } from "./confirm.mjs";
import { initPlatform } from "./platform-init.mjs";

export const platformHelp = `Administrator operations (separate Kubernetes credentials):
hibana platform init [directory]
hibana platform install --kubeconfig PATH --context NAME --overlay DIRECTORY --image IMAGE
hibana platform start|stop|status|uninstall --kubeconfig PATH --context NAME

Local platform development (explicit source checkout; Docker/kind required):
hibana platform install|start|stop|status|test --source CHECKOUT [--cluster hibana]
hibana platform uninstall --source CHECKOUT [--cluster hibana] --yes

Application profiles do not select Kubernetes targets. Use --dry-run to preview.
Remote install uses a prebuilt image; it never builds or creates a cluster.`;

export function platformCommand(args, options) {
  if (options.profile || options.url) throw new Error("Application --profile/--url do not select a Kubernetes target; use --kubeconfig and --context");
  const [action] = args;
  if (args.length !== 1 || !["install", "start", "stop", "status", "uninstall", "test"].includes(action)) throw new Error(platformHelp);
  const remote = Boolean(options.context || options.kubeconfig);
  if (remote && (!options.context || !options.kubeconfig)) throw new Error("Specify both --kubeconfig and --context for an existing cluster");
  if (remote && (options.cluster || options.local || options.source)) throw new Error("--source/--cluster/--local cannot be combined with an existing Kubernetes target");
  if (remote && action === "test") throw new Error("platform test uses the local development tenant only");
  if (remote && action === "install" && (!options.overlay || !options.image)) throw new Error("Existing-cluster installation requires --overlay and --image");
  if ((!remote || action !== "install") && (options.overlay || options.image)) throw new Error("--overlay and --image apply to existing-cluster installation only");
  if (!remote && !options.source) throw new Error("Select an existing cluster with --kubeconfig and --context, or local platform development with --source CHECKOUT");
  const target = remote ? `context ${options.context}, namespace hibana` : `local cluster ${options.cluster || "hibana"}`;
  const command = [remote ? fileURLToPath(new URL("../platform/remote.py", import.meta.url)) : resolve(options.source, "sdk/platform/kubernetes.py"), action];
  if (remote) command.push("--kubeconfig", resolve(options.kubeconfig), "--context", options.context);
  else command.push("--cluster", options.cluster || "hibana");
  if (options.overlay) command.push("--overlay", resolve(options.overlay));
  if (options.image) command.push("--image", options.image);
  if (options["dry-run"]) command.push("--dry-run");
  return { command, target, action };
}

export async function platform(args, options) {
  if (options.help || !args.length) { console.log(platformHelp); return; }
  if (args[0] === "init") return initPlatform(args[1]);
  const { command, target, action } = platformCommand(args, options);
  console.log(`${options["dry-run"] ? "Preview" : "Target"}: ${target}`);
  if (!options["dry-run"] && action === "uninstall" && !await confirm(`Uninstall Hibana from ${target}? ${options.context ? "External storage and the cluster are retained." : "Local cluster data will be removed."}`, options.yes)) { console.log("Cancelled."); return; }
  await run("python3", command);
}
