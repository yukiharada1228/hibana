import {
  readFile,
  mkdir,
  writeFile,
  rename,
  rm,
  chmod,
} from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";

export function profilesPath() {
  return join(
    process.env.HIBANA_CONFIG_HOME
      ? resolve(process.env.HIBANA_CONFIG_HOME)
      : join(
          process.env.XDG_CONFIG_HOME || join(homedir(), ".config"),
          "hibana",
        ),
    "profiles.json",
  );
}

export function profileName(name) {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/.test(name || ""))
    throw new Error(
      "Profile names must contain 1..64 letters, digits, underscores or hyphens",
    );
  return name;
}

export function serverUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error("Specify a valid Hibana management API URL");
  }
  if (url.username || url.password || url.search || url.hash)
    throw new Error(
      "Management API URL must not contain credentials, a query or a fragment",
    );
  if (
    url.protocol !== "https:" &&
    !(
      url.protocol === "http:" &&
      ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)
    )
  )
    throw new Error(
      "Hibana management API must use HTTPS (HTTP is allowed on loopback)",
    );
  return url.href.replace(/\/+$/, "");
}

export async function readProfiles(path = profilesPath()) {
  try {
    const value = JSON.parse(await readFile(path, "utf8"));
    if (
      !value ||
      !value.profiles ||
      typeof value.profiles !== "object" ||
      Array.isArray(value.profiles)
    )
      throw new Error("Invalid Hibana profiles file");
    return value;
  } catch (error) {
    if (error.code === "ENOENT") return { profiles: {} };
    throw error;
  }
}

async function writeProfiles(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, JSON.stringify(value, null, 2) + "\n", {
      flag: "wx",
      mode: 0o600,
    });
    await rename(temporary, path);
  } finally {
    await rm(temporary, { force: true });
  }
}

// Atomic rename protects readers; the lock also prevents concurrent commands
// from overwriting each other's changes with an older snapshot.
async function updateProfiles(change) {
  const path = profilesPath();
  const directory = resolve(path, "..");
  await mkdir(directory, { recursive: true, mode: 0o700 });
  await chmod(directory, 0o700);
  const lock = `${path}.lock`;
  const deadline = performance.now() + 5000;
  for (;;) {
    try {
      await mkdir(lock, { mode: 0o700 });
      break;
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      if (performance.now() >= deadline)
        throw new Error(
          `Profiles are busy. Retry after other Hibana commands finish. If a command crashed, remove ${lock} only after confirming no profile command is running.`,
        );
      await sleep(25);
    }
  }
  try {
    const state = await readProfiles(path);
    const result = change(state);
    await writeProfiles(path, state);
    return result;
  } finally {
    await rm(lock, { recursive: true, force: true });
  }
}

export async function saveProfile(name, connection) {
  profileName(name);
  await updateProfiles((state) => {
    Object.defineProperty(state.profiles, name, {
      value: connection,
      enumerable: true,
      configurable: true,
      writable: true,
    });
    state.current = name;
  });
}

export async function profileCommand([action, name, ...extra]) {
  if (
    extra.length ||
    !["list", "use", "remove"].includes(action) ||
    (action === "list" ? name !== undefined : !name)
  )
    throw new Error("Usage: hibana profile list | use NAME | remove NAME");
  if (action === "list") {
    const state = await readProfiles();
    for (const [key, value] of Object.entries(state.profiles))
      console.log(
        `${key === state.current ? "*" : " "} ${key}\t${value.url}\t${value.tenant || ""}\t${value.token ? "logged in" : "logged out"}`,
      );
    if (!Object.keys(state.profiles).length)
      console.log("No profiles. Run hibana login --url https://API_HOST");
    return;
  }
  profileName(name);
  const message = await updateProfiles((state) => {
    if (!Object.hasOwn(state.profiles, name))
      throw new Error(`Unknown profile: ${name}`);
    if (action === "use") state.current = name;
    else {
      delete state.profiles[name];
      if (state.current === name) delete state.current;
    }
    return action === "use"
      ? `Using ${name}: ${state.profiles[name].url}`
      : `Removed profile ${name}`;
  });
  console.log(message);
}

export async function logout(options = {}) {
  const name = await updateProfiles((state) => {
    const name = options.profile || process.env.HIBANA_PROFILE || state.current;
    if (!name || !Object.hasOwn(state.profiles, name))
      throw new Error("Select a saved profile with --profile NAME");
    delete state.profiles[name].token;
    return name;
  });
  console.log(`Logged out of ${name}. The locally saved token was removed.`);
}

export async function connection(options = {}) {
  const state = await readProfiles();
  const explicitProfile = options.profile || process.env.HIBANA_PROFILE;
  const name = explicitProfile || state.current;
  const saved =
    name && Object.hasOwn(state.profiles, name)
      ? state.profiles[name]
      : undefined;
  if (explicitProfile && !saved && !options.login)
    throw new Error(`Unknown profile: ${explicitProfile}`);
  const endpoint =
    options.url ||
    (options.profile ? saved?.url : undefined) ||
    process.env.HIBANA_URL ||
    saved?.url;
  if (!endpoint)
    throw new Error(
      "No Hibana server selected. Run hibana login --url https://API_HOST, or set HIBANA_URL",
    );
  const url = serverUrl(endpoint);
  const matches = saved?.url && serverUrl(saved.url) === url;
  return {
    url,
    profile: explicitProfile || (matches ? name : "default"),
    token:
      options.token ||
      process.env.HIBANA_TOKEN ||
      (matches ? saved.token : undefined),
    tenant:
      options.tenant ||
      process.env.HIBANA_TENANT ||
      (matches ? saved.tenant : undefined),
  };
}
