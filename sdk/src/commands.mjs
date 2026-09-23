import { parseArgs } from "node:util";

const string = (value, description, short) => ({
  type: "string",
  value,
  description,
  ...(short ? { short } : {}),
});
const boolean = (description, short) => ({
  type: "boolean",
  description,
  ...(short ? { short } : {}),
});
const options = {
  help: boolean("Show help for this command", "h"),
  json: boolean("Output the complete API response as JSON"),
  format: string(
    "FORMAT",
    "pretty or json (default: pretty in a terminal, json when piped)",
  ),
  status: string(
    "STATUS",
    "Filter invocation outcome: ok, error or canceled (HTTP status is separate)",
  ),
  search: string(
    "TEXT",
    "Match literal, case-sensitive text in stdout or stderr",
  ),
  "version-id": string("ID", "Filter by deployed version ID"),
  verbose: boolean("Include internal identifiers and the full version"),
  config: string("FILE", "Project configuration (default: hibana.json)", "c"),
  template: string("NAME", "hono (default), javascript, rust or go"),
  "no-install": boolean(
    "Create project files without installing npm dependencies",
  ),
  "cli-package": string("PATH", "Pin a local CLI package in the new project"),
  port: string("PORT", "Local HTTP port (default: 8787)"),
  "no-watch": boolean("Run once without watching for file changes"),
  "frozen-lockfile": boolean(
    "Require hibana-lock.json to match extension sources; never update it",
  ),
  runtime: string("PATH", "Use a specific local runtime executable"),
  profile: string("NAME", "Use a saved connection profile"),
  url: string("URL", "Hibana management API URL"),
  tenant: string("TEAM", "Tenant name"),
  "no-browser": boolean("Print the login URL without opening a browser"),
  version: string("VERSION", "Version to use"),
  all: boolean("Select all applications in the current tenant"),
  "all-tenants": boolean("Select every tenant; requires BOOTSTRAP_ADMIN_TOKEN"),
  "dry-run": boolean("Preview the operation without making changes"),
  yes: boolean("Confirm without prompting", "y"),
  from: string("FILE", "Install a runtime from a local file"),
  sha256: string("HASH", "SHA-256 checksum; required with --from"),
  kubeconfig: string("PATH", "Kubernetes credentials file"),
  context: string("NAME", "Kubernetes context"),
  overlay: string("DIRECTORY", "Kubernetes installation overlay"),
  image: string("IMAGE", "Prebuilt platform image"),
  source: string("CHECKOUT", "Source checkout for local platform development"),
  cluster: string("NAME", "Local development cluster (default: hibana)"),
  "with-keycloak": boolean(
    "Also create a separate Keycloak overlay with a persistent database",
  ),
};
const remote = ["profile", "url"];
const projectRemote = ["config", ...remote];
const connectionHelp =
  "Uses your selected profile. Override with --profile NAME or --url URL.\nCI: set HIBANA_URL and HIBANA_TOKEN. Sign in with hibana login.";
const leaf = (description, usage, flags = [], extra = {}) => ({
  description,
  usage,
  flags,
  min: 0,
  max: 0,
  ...extra,
});
const secret = (action, description) =>
  leaf(description, `hibana secret ${action} NAME`, projectRemote, {
    min: 1,
    max: 1,
    secretName: true,
    notes: connectionHelp,
  });
const platformFlags = ["kubeconfig", "context", "source", "cluster", "dry-run"];
const platformNotes =
  "Existing cluster: specify both --kubeconfig and --context.\nLocal development: use --source CHECKOUT (requires Docker/kind).\nApplication profiles do not select Kubernetes targets.";

// The same command definitions drive help and validation, so unsupported flags
// cannot silently reach an operation such as deploy or delete.
const commands = {
  init: leaf(
    "Create a new application",
    "hibana init [directory]",
    ["template", "no-install", "cli-package"],
    {
      max: 1,
      examples: ["hibana init my-api", "hibana init my-api --template rust"],
      notes:
        "Creates a Hono application by default and installs its npm dependencies.\nHono and JavaScript projects pin this CLI version in devDependencies.\nProject scripts use the local CLI; no global installation is needed.\nUse --cli-package PATH for an unpublished or offline CLI package.\nUse an empty directory; omit the directory to create files in the current one.",
    },
  ),
  dev: leaf(
    "Run your application locally and reload changes",
    "hibana dev",
    ["config", "port", "no-watch", "runtime", "frozen-lockfile"],
    {
      examples: ["hibana dev", "hibana dev --port 3000"],
      notes:
        "Open http://127.0.0.1:8787 (or your chosen port). Press Ctrl+C to stop.\nThe matching local runtime is installed automatically when needed and reused.\nFor a supplied executable, use --runtime PATH or HIBANA_RUNTIME_BIN.\nAllow local outbound connections with dev.allow_outbound in hibana.json (HOST:PORT).\nLocal Secrets belong in .dev.vars; deployment permissions and Secrets are separate.",
    },
  ),
  build: leaf(
    "Build a WebAssembly Component",
    "hibana build",
    ["config", "frozen-lockfile"],
    {
      examples: ["hibana build"],
      notes:
        "Builds the project without starting a runtime or contacting a Hibana server.\nDeclare extension names and sources in hibana.json. The CLI installs them into .hibana/ and records hibana-lock.json. Commit the lockfile; use --frozen-lockfile in CI.",
    },
  ),
  deploy: leaf(
    "Build and deploy your application",
    "hibana deploy",
    [...projectRemote, "version", "frozen-lockfile", "verbose"],
    {
      examples: [
        "hibana deploy",
        "hibana deploy --profile staging --version 1.0.0",
      ],
      notes:
        "Builds and activates a new version. A version is generated when --version is omitted.\n" +
        "Version names use 1..128 ASCII characters: start with a letter or digit, then letters, digits, '.', '_', '+', '-'.\n" +
        connectionHelp,
    },
  ),
  tail: leaf(
    "Watch live application executions and their output",
    "hibana tail [NAME]",
    [...projectRemote, "format", "status", "search", "version-id", "verbose"],
    {
      max: 1,
      examples: [
        "hibana tail",
        "hibana tail my-api --format pretty",
        "hibana tail my-api --status error",
        "hibana tail my-api --search 'connection failed'",
        "hibana tail my-api --format json",
      ],
      notes:
        "Uses hibana.json when NAME is omitted. Requires Read permission.\n" +
        "Starts watching now; completed invocations appear as they arrive. Press Ctrl+C to stop.\n" +
        "ok means the application completed, including HTTP 4xx/5xx. error includes traps and timeouts.\n" +
        "Hibana does not currently produce canceled invocations.\n" +
        "JSON emits one execution per line; connection messages and gap warnings go to stderr.\n" +
        "Live delivery is best effort. View stored logs in the Console's execution history.\n" +
        connectionHelp,
    },
  ),
  rollback: leaf(
    "Restore a previously deployed version",
    "hibana rollback",
    [...projectRemote, "version", "verbose"],
    {
      examples: ["hibana rollback", "hibana rollback --version 1.0.0"],
      notes:
        "Restores the previous version when --version is omitted.\n" +
        connectionHelp,
    },
  ),
  list: leaf(
    "List deployed applications",
    "hibana list",
    [...remote, "all-tenants", "json"],
    { notes: connectionHelp },
  ),
  delete: leaf(
    "Delete a deployed application",
    "hibana delete [NAME]",
    [...projectRemote, "all", "all-tenants", "dry-run", "yes"],
    {
      max: 1,
      examples: [
        "hibana delete my-api --dry-run",
        "hibana delete my-api",
        "hibana delete --all --yes",
      ],
      notes:
        "Uses the name in hibana.json when NAME is omitted. Prompts before deletion.\nUse --yes in scripts. --all-tenants requires --all and administrator credentials.\n" +
        connectionHelp,
    },
  ),
  login: leaf(
    "Sign in and save a connection profile",
    "hibana login",
    [...remote, "tenant", "no-browser"],
    {
      examples: ["hibana login --url https://api.example.com --tenant team"],
      notes:
        "Opens your organization's login page in a browser on this computer.\nUse --no-browser to open the printed URL yourself on the same computer.\nReuses saved connection details; HIBANA_TENANT also supplies the tenant.\nCI: use a scoped HIBANA_TOKEN.",
    },
  ),
  logout: leaf(
    "Remove the saved token for a profile",
    "hibana logout",
    ["profile"],
    { notes: "Uses the selected profile when --profile is omitted." },
  ),
  profile: {
    description: "Manage saved connections",
    actions: {
      list: leaf(
        "List saved profiles and the selected connection",
        "hibana profile list",
      ),
      use: leaf(
        "Select the connection for subsequent commands",
        "hibana profile use NAME",
        [],
        { min: 1, max: 1 },
      ),
      remove: leaf(
        "Remove a saved connection profile",
        "hibana profile remove NAME",
        [],
        { min: 1, max: 1 },
      ),
    },
  },
  egress: {
    description: "Manage application outbound destinations",
    actions: {
      list: leaf(
        "List allowed outbound destinations",
        "hibana egress list",
        [...projectRemote, "json"],
        {
          notes:
            "An empty allow_outbound list denies all outbound access.\n" +
            connectionHelp,
        },
      ),
      ...Object.fromEntries(
        ["allow", "deny"].map((action) => [
          action,
          leaf(
            `${action === "allow" ? "Allow" : "Revoke"} outbound destinations for every version`,
            `hibana egress ${action} HOST:PORT [HOST:PORT ...]`,
            projectRemote,
            {
              min: 1,
              max: 64,
              examples: [`hibana egress ${action} db.example.com:5432`],
              notes:
                "Requires administrator credentials. Applies to past, current and future versions.\nSpecify destinations without a URL, password or wildcard. IPv6: [address]:port.\n" +
                connectionHelp,
            },
          ),
        ]),
      ),
    },
  },
  secret: {
    description: "Manage application secrets",
    actions: {
      put: {
        ...secret("put", "Store a secret from standard input"),
        examples: ["hibana secret put API_KEY < secret.txt"],
        notes:
          "Add the name to hibana.json secrets, allow deployment, then run hibana deploy.\n" +
          connectionHelp,
      },
      list: leaf(
        "List application secrets",
        "hibana secret list",
        [...projectRemote, "json"],
        { notes: connectionHelp },
      ),
      delete: secret("delete", "Delete a stored secret"),
      "allow-deploy": secret(
        "allow-deploy",
        "Allow a secret to be included in future deployments",
      ),
      "deny-deploy": secret(
        "deny-deploy",
        "Prevent a secret from being included in future deployments",
      ),
    },
  },
  runtime: {
    description: "Prepare a runtime for offline use or a specific version",
    actions: {
      install: leaf(
        "Prepare a local runtime (dev installs it automatically)",
        "hibana runtime install",
        ["version", "from", "sha256"],
        {
          examples: [
            "hibana runtime install",
            "hibana runtime install --from /path/to/hibana-worker --sha256 HASH",
          ],
          notes:
            "Installs the CLI's version by default and verifies the SHA-256 checksum.\nUse --from and --sha256 together to install a file brought into an offline environment.",
        },
      ),
    },
  },
  platform: {
    description: "Administer a Hibana platform",
    actions: {
      init: leaf(
        "Create site configuration for an existing Kubernetes cluster",
        "hibana platform init [directory]",
        ["with-keycloak"],
        {
          max: 1,
          examples: [
            "hibana platform init my-site",
            "hibana platform init my-site --with-keycloak",
          ],
          notes:
            "Creates a portable overlay and private credential files. Fill in the site settings, then run platform install.\n--with-keycloak adds identity/ for separate deployment in namespace hibana-identity; see identity/README.md.\nThe default directory is hibana-platform. Existing files are never overwritten.",
        },
      ),
      ...Object.fromEntries(
        ["install", "start", "stop", "status", "uninstall", "test"].map(
          (action) => [
            action,
            leaf(
              `${action[0].toUpperCase() + action.slice(1)} the platform`,
              `hibana platform ${action}`,
              [
                ...platformFlags,
                ...(action === "install" ? ["overlay", "image"] : []),
                ...(action === "uninstall" ? ["yes"] : []),
              ],
              {
                notes:
                  platformNotes +
                  (action === "install"
                    ? "\nConfiguration, permissions and resource changes are checked before installation.\n--dry-run performs these checks without changing resources; a new namespace defers namespaced server validation."
                    : ""),
                examples: [
                  action === "install"
                    ? "hibana platform install --kubeconfig config --context onprem --overlay overlay --image registry.example.com/hibana:v1"
                    : action === "test"
                      ? "hibana platform test --source /path/to/hibana"
                      : `hibana platform ${action} --kubeconfig config --context onprem`,
                ],
              },
            ),
          ],
        ),
      ),
    },
  },
};

function rows(entries) {
  const width = Math.max(...entries.map(([label]) => label.length));
  return entries
    .map(([label, description]) => `  ${label.padEnd(width)}  ${description}`)
    .join("\n");
}
export function help(command, action) {
  if (!command)
    return `Hibana — develop and deploy WebAssembly applications.

Usage: hibana <command> [options]

Get started:
  hibana init my-api
  cd my-api
  npm run dev

Development:
${rows(["init", "dev", "build", "deploy"].map((name) => [name, commands[name].description]))}

Applications and connections:
${rows(["login", "logout", "list", "tail", "rollback", "delete", "secret", "egress", "profile"].map((name) => [name, commands[name].description]))}

Advanced:
${rows(["runtime", "platform"].map((name) => [name, commands[name].description]))}

Run hibana <command> --help for options and examples.
Use --profile NAME on remote commands to select a saved connection.
hibana --version shows the installed CLI version.`;
  const parent = commands[command],
    spec = action ? parent.actions[action] : parent;
  if (spec.actions)
    return `${spec.description}\n\nUsage: hibana ${command} <command>\n\nCommands:\n${rows(Object.entries(spec.actions).map(([name, item]) => [name, item.description]))}\n\nRun hibana ${command} <command> --help for options and examples.`;
  const flags = [...spec.flags, "help"];
  return `${spec.description}\n\nUsage: ${spec.usage} [options]\n\nOptions:\n${rows(
    flags.map((name) => {
      const option = options[name];
      return [
        `${option.short ? `-${option.short}, ` : ""}--${name}${option.value ? ` ${option.value}` : ""}`,
        option.description,
      ];
    }),
  )}${spec.examples ? `\n\nExamples:\n${spec.examples.map((example) => `  ${example}`).join("\n")}` : ""}${spec.notes ? `\n\n${spec.notes}` : ""}`;
}

function suggestion(input, choices) {
  // A small edit-distance suggestion; never execute a guessed command.
  function distance(a, b) {
    let row = Array.from({ length: b.length + 1 }, (_, i) => i);
    for (let i = 0; i < a.length; i++) {
      const next = [i + 1];
      for (let j = 0; j < b.length; j++)
        next.push(
          Math.min(next[j] + 1, row[j + 1] + 1, row[j] + (a[i] !== b[j])),
        );
      row = next;
    }
    return row[b.length];
  }
  const ranked = choices
    .map((choice) => [choice, distance(input, choice)])
    .sort((a, b) => a[1] - b[1]);
  return ranked[0]?.[1] <= (input.length > 4 ? 2 : 1)
    ? ` Did you mean '${ranked[0][0]}'?`
    : "";
}

export function parseCommand(argv) {
  if (argv.length === 1 && ["--version", "-v"].includes(argv[0]))
    return { versionOnly: true };
  let context = "",
    parsed;
  try {
    try {
      parsed = parseArgs({ args: argv, options, allowPositionals: true });
    } catch (error) {
      if (error.code === "ERR_PARSE_ARGS_UNKNOWN_OPTION") {
        const unknown = /Unknown option '([^']+)'/.exec(error.message)?.[1];
        if (unknown)
          throw new Error(
            `Unknown option '${unknown}'.${suggestion(
              unknown,
              Object.keys(options).map((name) => `--${name}`),
            )}`,
          );
      }
      throw error;
    }
    const { values } = parsed;
    let [command, ...args] = parsed.positionals;
    if (command === "help") {
      [command, ...args] = args;
      values.help = true;
    }
    if (!command) {
      if (Object.keys(values).some((name) => name !== "help"))
        throw new Error("Choose a command before specifying its options.");
      return { helpText: help() };
    }
    if (!Object.hasOwn(commands, command))
      throw new Error(
        `Unknown command '${command}'.${suggestion(command, Object.keys(commands))}`,
      );
    context = command;
    let spec = commands[command],
      action;
    if (spec.actions) {
      [action] = args;
      if (action) {
        if (!Object.hasOwn(spec.actions, action))
          throw new Error(
            `Unknown ${command} command '${action}'.${suggestion(action, Object.keys(spec.actions))}`,
          );
        spec = spec.actions[action];
        context += ` ${action}`;
      }
    }
    const allowed = new Set([...(spec.flags || []), "help"]);
    for (const [name, value] of Object.entries(values)) {
      if (!allowed.has(name))
        throw new Error(
          `Option '--${name}' is not supported by 'hibana ${context}'.`,
        );
      if (typeof value === "string" && !value.trim())
        throw new Error(
          `Option '--${name}' requires a non-empty ${options[name].value}.`,
        );
    }
    if (values.help || spec.actions) return { helpText: help(command, action) };
    const positional = action ? args.slice(1) : args;
    if (positional.length < spec.min || positional.length > spec.max)
      throw new Error(`Usage: ${spec.usage} [options]`);
    if (spec.secretName && !/^[A-Z_][A-Z0-9_]{0,63}$/.test(positional[0]))
      throw new Error(
        "Secret names must contain 1..64 uppercase letters, digits or underscores, starting with a letter or underscore.",
      );
    return { command, args, values };
  } catch (error) {
    error.message += `\nRun hibana${context ? ` ${context}` : ""} --help for usage.`;
    throw error;
  }
}
