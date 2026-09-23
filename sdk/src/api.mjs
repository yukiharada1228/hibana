import { readFile } from "node:fs/promises";

import { connection, saveProfile } from "./profiles.mjs";
import { validateVersionName } from "./version-name.mjs";
import { quote } from "./output.mjs";

export class ApiError extends Error {
  constructor(method, path, status) {
    super(`${method} ${path}: HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
  }
}

export async function apiClient(options = {}) {
  const { token: savedToken, ...selected } = await connection(options);
  const { url } = selected;
  let token = savedToken;
  async function request(
    path,
    { method = "GET", body, auth = true, signal } = {},
  ) {
    if (auth && !token)
      throw new Error("Run hibana login, or set HIBANA_TOKEN");
    const headers = auth ? { Authorization: `Bearer ${token}` } : {};
    if (body !== undefined && !(body instanceof FormData)) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(body);
    }
    const deadline = AbortSignal.timeout(120000);
    const response = await fetch(url + path, {
      method,
      headers,
      body,
      redirect: "error",
      signal: signal ? AbortSignal.any([signal, deadline]) : deadline,
    });
    // Error bodies can contain user values; do not copy them into terminal/CI logs.
    if (!response.ok) {
      await response.body?.cancel().catch(() => {});
      const error = new ApiError(method, path, response.status);
      const login = `hibana login --profile ${quote(selected.profile)} --url ${quote(url)}`;
      if (response.status === 401)
        error.hint = auth
          ? `Authentication failed or expired (HTTP 401). Run ${login} to sign in again. If using HIBANA_TOKEN, replace the expired token.`
          : "Login failed (HTTP 401). Check the tenant and your organization account.";
      else if (response.status === 403)
        error.hint =
          "Permission denied (HTTP 403). Sign in with an account authorized for this operation.";
      else if (response.status === 503)
        error.hint =
          "The platform is unavailable (HTTP 503). Check its status before retrying.";
      throw error;
    }
    if (response.status === 204) return;
    try {
      return await response.json();
    } catch (cause) {
      // Never confirm success from a malformed or incomplete response. Parser
      // errors can include response values, so report only the endpoint/status.
      const error = new Error(
        `${method} ${path}: Invalid JSON response (HTTP ${response.status}). Check the operation's status before retrying.`,
      );
      // Read-only live polling may retry a broken response stream. Do not retain
      // the original parser error: it can contain response values. Mutation
      // callers still never retry an ambiguous operation automatically.
      error.retryableRead = [
        "TypeError",
        "AbortError",
        "TimeoutError",
      ].includes(cause?.name);
      throw error;
    }
  }
  return {
    ...selected,
    request,
    async loginOidc(options = {}) {
      const { browserLogin } = await import("./oidc.mjs");
      const result = await browserLogin({ ...selected, request }, options);
      if (typeof result.token !== "string" || !result.token)
        throw new Error("Login returned no access token");
      const { profile, ...value } = selected;
      await saveProfile(profile, { ...value, token: result.token });
      token = result.token;
      return profile;
    },
  };
}

export async function findComponent(api, name, options) {
  const result = await api.request("/components", options);
  return result.find((item) => item.name === name);
}

export async function deploy(api, config, artifact, version) {
  validateVersionName(version);
  let component = await findComponent(api, config.name);
  if (!component) {
    try {
      component = await api.request("/components", {
        method: "POST",
        body: { name: config.name },
      });
    } catch (error) {
      if (!(error instanceof ApiError) || error.status !== 409) throw error;
      // Another first deploy may have created this application after our read.
      // Reuse it once; neither creation nor version uploads are retried.
      component = await findComponent(api, config.name);
      if (!component) throw error;
    }
  }
  const id = component.component_id;
  const base = `/components/${encodeURIComponent(id)}`;
  const form = new FormData();
  form.set("version", version);
  form.set("activate", "true");
  form.set("ingress", "true");
  form.set("vars", JSON.stringify(config.vars));
  form.set("secrets", JSON.stringify(config.secrets ?? []));
  form.set("resource_limits", JSON.stringify(config.resources));
  form.set(
    "wasm",
    new Blob([await readFile(artifact)], { type: "application/wasm" }),
    "component.wasm",
  );
  const result = await api.request(`${base}/versions`, {
    method: "POST",
    body: form,
  });
  return { ...result, component_id: id, version };
}

export async function rollback(api, config, version) {
  const component = await findComponent(api, config.name);
  if (!component) throw new Error("Application has not been deployed");
  const id = component.component_id;
  return api.request(`/components/${encodeURIComponent(id)}/rollback`, {
    method: "POST",
    body: version === undefined ? {} : { version },
  });
}

// Inventory operations may explicitly use platform-administrator credentials.
// Ordinary operations never pick up BOOTSTRAP_ADMIN_TOKEN implicitly.
export function inventoryClient(options) {
  if (options["all-tenants"] && !process.env.BOOTSTRAP_ADMIN_TOKEN) {
    throw new Error(
      "--all-tenants requires BOOTSTRAP_ADMIN_TOKEN (platform administrator)",
    );
  }
  return apiClient({
    ...options,
    token: options["all-tenants"]
      ? process.env.BOOTSTRAP_ADMIN_TOKEN
      : undefined,
  });
}
