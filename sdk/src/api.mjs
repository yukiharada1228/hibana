import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import { join } from "node:path";

import { connection, saveProfile } from "./profiles.mjs";

export async function apiClient(root, options = {}) {
  const { token: savedToken, ...selected } = await connection(root, options);
  const { url } = selected;
  let token = savedToken;
  async function request(path, { method = "GET", body, auth = true, signal } = {}) {
    if (auth && !token) throw new Error("Run hibana login, or set HIBANA_TOKEN");
    const headers = auth ? { Authorization: `Bearer ${token}` } : {};
    if (body !== undefined && !(body instanceof FormData)) { headers["Content-Type"] = "application/json"; body = JSON.stringify(body); }
    const deadline = AbortSignal.timeout(120000);
    const response = await fetch(url + path, { method, headers, body, redirect: "error", signal: signal ? AbortSignal.any([signal, deadline]) : deadline });
    const text = await response.text();
    let result;
    try { result = text ? JSON.parse(text) : {}; } catch { result = {}; }
    // Error bodies can contain user values; do not copy them into terminal/CI logs.
    if (!response.ok) throw new Error(`${method} ${path}: HTTP ${response.status}`);
    return result;
  }
  return { ...selected, request, async login({ password = process.env.HIBANA_PASSWORD, persist = "project" } = {}) {
    const { tenant, email } = selected;
    if (!tenant || !email || !password) throw new Error("Specify --tenant, --email and --password-stdin (or HIBANA_TENANT, HIBANA_EMAIL and HIBANA_PASSWORD) to log in");
    const result = await request("/auth/login", { method: "POST", auth: false, body: { tenant_slug: tenant, email, password } });
    token = result.token;
    if (typeof token !== "string" || !token) throw new Error("Login returned no access token");
    if (persist === "profile") {
      const { profile, ...value } = selected;
      await saveProfile(profile, { ...value, token });
      return profile;
    }
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
  form.set("activate", "true");
  form.set("ingress", "true");
  form.set("vars", JSON.stringify(config.vars));
  form.set("secrets", JSON.stringify(config.secrets ?? []));
  form.set("resource_limits", JSON.stringify(config.resources));
  form.set("wasm", new Blob([await readFile(artifact)], { type: "application/wasm" }), "component.wasm");
  await api.request(`${base}/versions`, { method: "POST", body: form });
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
