import { parseArgs } from "node:util";

const string = (value, description, short) => ({ type: "string", value, description, ...(short ? { short } : {}) });
const boolean = (description, short) => ({ type: "boolean", description, ...(short ? { short } : {}) });
const options = {
  help: boolean("Show help for this command", "h"),
  config: string("FILE", "Project configuration (default: hibana.json)", "c"),
  template: string("NAME", "hono (default), javascript, rust or go"),
  "no-install": boolean("Create project files without installing npm dependencies"),
  "cli-package": string("PATH", "Use a local CLI package when creating a project"),
  port: string("PORT", "Local HTTP port (default: 8787)"),
  "no-watch": boolean("Run once without watching for file changes"),
  runtime: string("PATH", "Use a specific local runtime executable"),
  profile: string("NAME", "Use a saved connection profile"),
  url: string("URL", "Hibana management API URL"),
  tenant: string("TEAM", "Tenant name"),
  email: string("EMAIL", "Account email"),
  "ingress-domain": string("DOMAIN", "Application domain, for example apps.example.com"),
  "password-stdin": boolean("Read the login password from standard input"),
  version: string("VERSION", "Version to use"),
  name: string("NAME", "Application name (alternative to the positional name)"),
  all: boolean("Select all applications in the current tenant"),
  "all-tenants": boolean("Select every tenant; requires BOOTSTRAP_ADMIN_TOKEN"),
  "dry-run": boolean("Preview the operation without making changes"),
  yes: boolean("Confirm without prompting", "y"),
  force: boolean("Alias for --yes"),
  from: string("FILE", "Install a runtime from a local file"),
  sha256: string("HASH", "SHA-256 checksum; required with --from"),
  kubeconfig: string("PATH", "Kubernetes credentials file"),
  context: string("NAME", "Kubernetes context"),
  overlay: string("DIRECTORY", "Kubernetes installation overlay"),
  image: string("IMAGE", "Prebuilt platform image"),
  source: string("CHECKOUT", "Source checkout for local platform development"),
  cluster: string("NAME", "Local development cluster (default: hibana)"),
  local: boolean("Local platform development; requires --source"),
};
const remote = ["profile", "url"];
const projectRemote = ["config", ...remote];
const connectionHelp = "Uses your selected profile. Override with --profile NAME or --url URL.\nCI: set HIBANA_URL and HIBANA_TOKEN. Sign in with hibana login.";
const leaf = (description, usage, flags = [], extra = {}) => ({ description, usage, flags, min: 0, max: 0, ...extra });
const secret = (action, description) => leaf(description, `hibana secret ${action} NAME`, projectRemote, {
  min: 1, max: 1, secretName: true, notes: connectionHelp,
});
const platformFlags = ["kubeconfig", "context", "source", "cluster", "local", "dry-run"];
const platformNotes = "Existing cluster: specify both --kubeconfig and --context.\nLocal development: use --source CHECKOUT (requires Docker/kind).\nApplication profiles do not select Kubernetes targets.";

// The same command definitions drive help and validation, so unsupported flags
// cannot silently reach an operation such as deploy or delete.
const commands = {
  init: leaf("Create a new application", "hibana init [directory]", ["template", "no-install", "cli-package"], {
    max: 1, examples: ["hibana init my-api", "hibana init my-api --template rust"],
    notes: "Creates a Hono application by default and installs its npm dependencies.\nUse an empty directory; omit the directory to create files in the current one.",
  }),
  dev: leaf("Run your application locally and reload changes", "hibana dev", ["config", "port", "no-watch", "runtime"], {
    examples: ["hibana dev", "hibana dev --port 3000"],
    notes: "Open http://127.0.0.1:8787 (or your chosen port). Press Ctrl+C to stop.\nThe matching local runtime is installed automatically when needed and reused.\nFor a supplied executable, use --runtime PATH or HIBANA_RUNTIME_BIN.",
  }),
  build: leaf("Build a WebAssembly Component", "hibana build", ["config"], {
    examples: ["hibana build"], notes: "Builds the project without starting a runtime or contacting a Hibana server.",
  }),
  deploy: leaf("Build and deploy your application", "hibana deploy", [...projectRemote, "version", "tenant", "ingress-domain"], {
    examples: ["hibana deploy", "hibana deploy --profile staging --version 1.0.0"],
    notes: "Builds and activates a new version. A version is generated when --version is omitted.\n" + connectionHelp,
  }),
  rollback: leaf("Restore a previously deployed version", "hibana rollback", [...projectRemote, "version"], {
    examples: ["hibana rollback", "hibana rollback --version 1.0.0"],
    notes: "Restores the previous version when --version is omitted.\n" + connectionHelp,
  }),
  list: leaf("List deployed applications as JSON", "hibana list", [...projectRemote, "all-tenants"], { notes: connectionHelp }),
  delete: leaf("Delete a deployed application", "hibana delete [NAME]", [...projectRemote, "name", "all", "all-tenants", "dry-run", "yes", "force"], {
    max: 1, examples: ["hibana delete my-api --dry-run", "hibana delete my-api", "hibana delete --all --yes"],
    notes: "Uses the name in hibana.json when NAME is omitted. Prompts before deletion.\nUse --yes in scripts. --all-tenants requires --all and administrator credentials.\n" + connectionHelp,
  }),
  login: leaf("Sign in and save a connection profile", "hibana login", [...remote, "tenant", "email", "ingress-domain", "password-stdin"], {
    examples: ["hibana login --profile onprem --url https://api.example.com --tenant team --email dev@example.com --password-stdin < password.txt"],
    notes: "Reuses saved connection details when signing in again.\nSupply the password through --password-stdin or HIBANA_PASSWORD.\nHIBANA_TENANT and HIBANA_EMAIL can also supply account details.",
  }),
  logout: leaf("Remove the saved token for a profile", "hibana logout", ["profile"], { notes: "Uses the selected profile when --profile is omitted." }),
  profile: {
    description: "Manage saved connections",
    actions: {
      list: leaf("List saved profiles and the selected connection", "hibana profile list"),
      use: leaf("Select the connection for subsequent commands", "hibana profile use NAME", [], { min: 1, max: 1 }),
      remove: leaf("Remove a saved connection profile", "hibana profile remove NAME", [], { min: 1, max: 1 }),
    },
  },
  secret: {
    description: "Manage application secrets",
    actions: {
      put: { ...secret("put", "Store a secret from standard input"), examples: ["hibana secret put API_KEY < secret.txt"], notes: "Add the name to hibana.json secrets, allow deployment, then run hibana deploy.\n" + connectionHelp },
      list: leaf("List application secrets as JSON", "hibana secret list", projectRemote, { notes: connectionHelp }),
      delete: secret("delete", "Delete a stored secret"),
      "allow-deploy": secret("allow-deploy", "Allow a secret to be included in future deployments"),
      "deny-deploy": secret("deny-deploy", "Prevent a secret from being included in future deployments"),
    },
  },
  runtime: {
    description: "Prepare a runtime for offline use or a specific version",
    actions: {
      install: leaf("Prepare a local runtime (dev installs it automatically)", "hibana runtime install", ["version", "from", "sha256"], {
        examples: ["hibana runtime install", "hibana runtime install --from /path/to/hibana-worker --sha256 HASH"],
        notes: "Installs the CLI's version by default and verifies the SHA-256 checksum.\nUse --from and --sha256 together to install a file brought into an offline environment.",
      }),
    },
  },
  platform: {
    description: "Administer a Hibana platform",
    actions: { init: leaf("Create site configuration for an existing Kubernetes cluster", "hibana platform init [directory]", [], {
      max: 1, examples: ["hibana platform init my-site"], notes: "Creates a portable overlay and private credential files. Fill in the site settings, then run platform install.\nThe default directory is hibana-platform. Existing files are never overwritten.",
    }), ...Object.fromEntries(["install", "start", "stop", "status", "uninstall", "test"].map(action => [action,
      leaf(`${action[0].toUpperCase() + action.slice(1)} the platform`, `hibana platform ${action}`, [
        ...platformFlags, ...(action === "install" ? ["overlay", "image"] : []), ...(action === "uninstall" ? ["yes"] : []),
      ], { notes: platformNotes + (action === "install" ? "\nConfiguration, permissions and resource changes are checked before installation.\n--dry-run performs these checks without changing resources; a new namespace defers namespaced server validation." : ""), examples: [action === "install"
        ? "hibana platform install --kubeconfig config --context onprem --overlay overlay --image registry.example.com/hibana:v1"
        : action === "test" ? "hibana platform test --source /path/to/hibana"
        : `hibana platform ${action} --kubeconfig config --context onprem`] }),
    ])) },
  },
};

function rows(entries) {
  const width = Math.max(...entries.map(([label]) => label.length));
  return entries.map(([label, description]) => `  ${label.padEnd(width)}  ${description}`).join("\n");
}
export function help(command, action) {
  if (!command) return `Hibana — develop and deploy WebAssembly applications.

Usage: hibana <command> [options]

Get started:
  hibana init my-api
  cd my-api
  npm run dev

Development:
${rows(["init", "dev", "build", "deploy"].map(name => [name, commands[name].description]))}

Applications and connections:
${rows(["login", "logout", "list", "rollback", "delete", "secret", "profile"].map(name => [name, commands[name].description]))}

Advanced:
${rows(["runtime", "platform"].map(name => [name, commands[name].description]))}

Run hibana <command> --help for options and examples.
Use --profile NAME on remote commands to select a saved connection.
hibana --version shows the installed CLI version.`;
  const parent = commands[command], spec = action ? parent.actions[action] : parent;
  if (spec.actions) return `${spec.description}\n\nUsage: hibana ${command} <command>\n\nCommands:\n${rows(Object.entries(spec.actions).map(([name, item]) => [name, item.description]))}\n\nRun hibana ${command} <command> --help for options and examples.`;
  const flags = [...spec.flags, "help"].filter(name => !["force", "local"].includes(name));
  return `${spec.description}\n\nUsage: ${spec.usage} [options]\n\nOptions:\n${rows(flags.map(name => {
    const option = options[name];
    return [`${option.short ? `-${option.short}, ` : ""}--${name}${option.value ? ` ${option.value}` : ""}`, option.description];
  }))}${spec.examples ? `\n\nExamples:\n${spec.examples.map(example => `  ${example}`).join("\n")}` : ""}${spec.notes ? `\n\n${spec.notes}` : ""}`;
}

function suggestion(input, choices) {
  // A small edit-distance suggestion; never execute a guessed command.
  function distance(a, b) {
    let row = Array.from({ length: b.length + 1 }, (_, i) => i);
    for (let i = 0; i < a.length; i++) {
      const next = [i + 1];
      for (let j = 0; j < b.length; j++) next.push(Math.min(next[j] + 1, row[j + 1] + 1, row[j] + (a[i] !== b[j])));
      row = next;
    }
    return row[b.length];
  }
  const ranked = choices.map(choice => [choice, distance(input, choice)]).sort((a, b) => a[1] - b[1]);
  return ranked[0]?.[1] <= (input.length > 4 ? 2 : 1) ? ` Did you mean '${ranked[0][0]}'?` : "";
}

export function parseCommand(argv) {
  if (argv.length === 1 && ["--version", "-v"].includes(argv[0])) return { versionOnly: true };
  let context = "", parsed;
  try {
    try {
      parsed = parseArgs({ args: argv, options, allowPositionals: true });
    } catch (error) {
      if (error.code === "ERR_PARSE_ARGS_UNKNOWN_OPTION") {
        const unknown = /Unknown option '([^']+)'/.exec(error.message)?.[1];
        if (unknown) throw new Error(`Unknown option '${unknown}'.${suggestion(unknown, Object.keys(options).map(name => `--${name}`))}`);
      }
      throw error;
    }
    const { values } = parsed;
    let [command, ...args] = parsed.positionals;
    if (command === "help") { [command, ...args] = args; values.help = true; }
    if (!command) {
      if (Object.keys(values).some(name => name !== "help")) throw new Error("Choose a command before specifying its options.");
      return { helpText: help() };
    }
    if (!Object.hasOwn(commands, command)) throw new Error(`Unknown command '${command}'.${suggestion(command, Object.keys(commands))}`);
    context = command;
    let spec = commands[command], action;
    if (spec.actions) {
      [action] = args;
      if (action) {
        if (!Object.hasOwn(spec.actions, action)) throw new Error(`Unknown ${command} command '${action}'.${suggestion(action, Object.keys(spec.actions))}`);
        spec = spec.actions[action]; context += ` ${action}`;
      }
    }
    const allowed = new Set([...(spec.flags || []), "help"]);
    for (const [name, value] of Object.entries(values)) {
      if (!allowed.has(name)) throw new Error(`Option '--${name}' is not supported by 'hibana ${context}'.`);
      if (typeof value === "string" && !value.trim()) throw new Error(`Option '--${name}' requires a non-empty ${options[name].value}.`);
    }
    if (values.help || spec.actions) return { helpText: help(command, action) };
    const positional = action ? args.slice(1) : args;
    if (positional.length < spec.min || positional.length > spec.max) throw new Error(`Usage: ${spec.usage} [options]`);
    if (spec.secretName && !/^[A-Z_][A-Z0-9_]{0,63}$/.test(positional[0])) throw new Error("Secret names must contain 1..64 uppercase letters, digits or underscores, starting with a letter or underscore.");
    return { command, args, values };
  } catch (error) {
    error.message += `\nRun hibana${context ? ` ${context}` : ""} --help for usage.`;
    throw error;
  }
}
