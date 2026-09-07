import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import { join } from "node:path";

export async function apiClient(root) {
  let saved = {};
  try { saved = JSON.parse(await readFile(join(root, ".hibana/auth.json"), "utf8")); }
  catch (error) { if (error.code !== "ENOENT") throw error; }
  const url = (process.env.HIBANA_URL || saved.url || "http://127.0.0.1:8080").replace(/\/$/, "");
  const parsed = new URL(url);
  if (parsed.protocol !== "https:" && !(parsed.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(parsed.hostname))) throw new Error("HIBANA_URL must use HTTPS (HTTP is allowed on loopback)");
  // Never reuse a saved token for a different server.
  let token = process.env.HIBANA_TOKEN || (saved.url === url ? saved.token : undefined);
  async function request(path, { method = "GET", body, auth = true } = {}) {
    if (auth && !token) throw new Error("Run hibana login, or set HIBANA_TOKEN");
    const headers = auth ? { Authorization: `Bearer ${token}` } : {};
    if (body !== undefined && !(body instanceof FormData)) { headers["Content-Type"] = "application/json"; body = JSON.stringify(body); }
    const response = await fetch(url + path, { method, headers, body, redirect: "error", signal: AbortSignal.timeout(120000) });
    const text = await response.text();
    let result;
    try { result = text ? JSON.parse(text) : {}; } catch { result = {}; }
    // Error bodies can contain user values; do not copy them into terminal/CI logs.
    if (!response.ok) throw new Error(`${method} ${path}: HTTP ${response.status}`);
    return result;
  }
  return { request, async login() {
    const { HIBANA_TENANT: tenant, HIBANA_EMAIL: email, HIBANA_PASSWORD: password } = process.env;
    if (!tenant || !email || !password) throw new Error("Set HIBANA_TENANT, HIBANA_EMAIL and HIBANA_PASSWORD to log in");
    const result = await request("/auth/login", { method: "POST", auth: false, body: { tenant_slug: tenant, email, password } });
    token = result.token;
    if (typeof token !== "string" || !token) throw new Error("Login returned no access token");
    await mkdir(join(root, ".hibana"), { recursive: true, mode: 0o700 });
    const path = join(root, ".hibana/auth.json");
    await writeFile(path, JSON.stringify({ url, token }) + "\n", { mode: 0o600 });
    await chmod(path, 0o600);
  } };
}

export async function findComponent(api, name) {
  const result = await api.request("/components");
  return (Array.isArray(result) ? result : result.components).find(item => item.name === name);
}

export async function deploy(api, config, artifact, version) {
  let component = await findComponent(api, config.name);
  if (!component) component = await api.request("/components", { method: "POST", body: { name: config.name } });
  const id = component.component_id || component.id;
  const base = `/components/${encodeURIComponent(id)}`;
  const form = new FormData();
  form.set("version", version);
  form.set("activate", "false");
  form.set("resource_limits", JSON.stringify(config.resources));
  form.set("wasm", new Blob([await readFile(artifact)], { type: "application/wasm" }), "component.wasm");
  await api.request(`${base}/versions`, { method: "POST", body: form });
  // Resolve all environment grants before making this version active.
  const secrets = await api.request(`${base}/secrets/keys`);
  await api.request(`${base}/config`, { method: "PUT", body: { env: config.vars } });
  await api.request(`${base}/versions/${encodeURIComponent(version)}/capabilities`, { method: "PUT", body: { env: [...new Set([...Object.keys(config.vars), ...secrets.secrets.map(s => s.name)])] } });
  await api.request(`${base}/active-version`, { method: "PUT", body: { version } });
  await api.request(`${base}/ingress`, { method: "PUT", body: { enabled: true } });
  return { component_id: id, version };
}

export async function rollback(api, config, version) {
  const component = await findComponent(api, config.name);
  if (!component) throw new Error("Application has not been deployed");
  const id = component.component_id || component.id;
  return api.request(`/components/${encodeURIComponent(id)}/rollback`, {
    method: "POST", body: version === undefined ? {} : { version },
  });
}
