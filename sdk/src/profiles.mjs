import { readFile, mkdir, writeFile, rename, rm, chmod } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

export function profilesPath() {
  return join(process.env.HIBANA_CONFIG_HOME ? resolve(process.env.HIBANA_CONFIG_HOME) : join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "hibana"), "profiles.json");
}

export function profileName(name) {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/.test(name || "")) throw new Error("Profile names must contain 1..64 letters, digits, underscores or hyphens");
  return name;
}

export function serverUrl(value) {
  let url;
  try { url = new URL(value); } catch { throw new Error("Specify a valid Hibana management API URL"); }
  if (url.username || url.password || url.search || url.hash) throw new Error("Management API URL must not contain credentials, a query or a fragment");
  if (url.protocol !== "https:" && !(url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname))) throw new Error("Hibana management API must use HTTPS (HTTP is allowed on loopback)");
  return url.href.replace(/\/+$/, "");
}

export async function readProfiles() {
  try {
    const value = JSON.parse(await readFile(profilesPath(), "utf8"));
    if (!value || !value.profiles || typeof value.profiles !== "object" || Array.isArray(value.profiles)) throw new Error("Invalid Hibana profiles file");
    return value;
  } catch (error) { if (error.code === "ENOENT") return { profiles: {} }; throw error; }
}

async function writeProfiles(value) {
  const path = profilesPath();
  const directory = resolve(path, "..");
  await mkdir(directory, { recursive: true, mode: 0o700 });
  await chmod(directory, 0o700);
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, JSON.stringify(value, null, 2) + "\n", { flag: "wx", mode: 0o600 });
    await rename(temporary, path);
  } finally { await rm(temporary, { force: true }); }
}

export async function saveProfile(name, connection) {
  profileName(name);
  const state = await readProfiles();
  Object.defineProperty(state.profiles, name, { value: connection, enumerable: true, configurable: true, writable: true });
  state.current = name;
  await writeProfiles(state);
}

export async function profileCommand([action, name, ...extra]) {
  if (extra.length || !["list", "use", "remove"].includes(action) || (action === "list" ? name !== undefined : !name)) throw new Error("Usage: hibana profile list | use NAME | remove NAME");
  const state = await readProfiles();
  if (action === "list") {
    for (const [key, value] of Object.entries(state.profiles)) console.log(`${key === state.current ? "*" : " "} ${key}\t${value.url}\t${value.tenant || ""}\t${value.token ? "logged in" : "logged out"}`);
    if (!Object.keys(state.profiles).length) console.log("No profiles. Run hibana login --profile NAME --url https://API_HOST");
    return;
  }
  profileName(name);
  if (!Object.hasOwn(state.profiles, name)) throw new Error(`Unknown profile: ${name}`);
  if (action === "use") state.current = name;
  else { delete state.profiles[name]; if (state.current === name) delete state.current; }
  await writeProfiles(state);
  console.log(action === "use" ? `Using ${name}: ${state.profiles[name].url}` : `Removed profile ${name}`);
}

export async function logout(options = {}) {
  const state = await readProfiles();
  const name = options.profile || process.env.HIBANA_PROFILE || state.current;
  if (!name || !Object.hasOwn(state.profiles, name)) throw new Error("Select a saved profile with --profile NAME");
  delete state.profiles[name].token;
  await writeProfiles(state);
  console.log(`Logged out of ${name}. The locally saved token was removed.`);
}

export async function connection(root, options = {}) {
  const state = await readProfiles();
  const explicitProfile = options.profile || process.env.HIBANA_PROFILE;
  const name = explicitProfile || state.current;
  const saved = name && Object.hasOwn(state.profiles, name) ? state.profiles[name] : undefined;
  if (explicitProfile && !saved && !options.login) throw new Error(`Unknown profile: ${explicitProfile}`);
  let legacy = {};
  // Existing project credentials remain usable, but never override an explicit profile.
  if (!explicitProfile && root) {
    try { legacy = JSON.parse(await readFile(join(root, ".hibana/auth.json"), "utf8")); }
    catch (error) { if (error.code !== "ENOENT") throw error; }
  }
  const endpoint = options.url || (options.profile ? saved?.url : undefined) || process.env.HIBANA_URL || saved?.url || legacy.url;
  if (!endpoint) throw new Error("No Hibana server selected. Run hibana login --profile NAME --url https://API_HOST, or set HIBANA_URL");
  const url = serverUrl(endpoint);
  const selected = saved?.url && serverUrl(saved.url) === url ? saved : legacy;
  const matches = selected.url && serverUrl(selected.url) === url;
  return {
    url, profile: explicitProfile || (saved?.url && serverUrl(saved.url) === url ? name : "default"),
    token: options.token || process.env.HIBANA_TOKEN || (matches ? selected.token : undefined),
    tenant: options.tenant || process.env.HIBANA_TENANT || (matches ? selected.tenant : undefined),
    email: options.email || process.env.HIBANA_EMAIL || (matches ? selected.email : undefined),
    ingressDomain: options["ingress-domain"] || process.env.HIBANA_INGRESS_DOMAIN || (matches ? selected.ingressDomain : undefined),
  };
}
