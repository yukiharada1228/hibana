import { Api, type LoginOptions } from "./api";

const storageKey = "hibana.oidc.pending";
const random = () =>
  Array.from(crypto.getRandomValues(new Uint8Array(32)), (n) =>
    n.toString(16).padStart(2, "0"),
  ).join("");

export async function beginOidc(
  api: Api,
  tenant: string,
  options: LoginOptions,
) {
  const redirect = new URL(options.console_url || "");
  if (
    redirect.origin !== location.origin ||
    redirect.pathname !== location.pathname
  )
    throw new Error(
      "認証の戻り先とこのコンソールのURLが一致していません。管理者へお問い合わせください。",
    );
  const verifier = random(),
    state = random();
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(verifier),
  );
  const challenge = btoa(String.fromCharCode(...new Uint8Array(digest)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
  const result = await api.beginOidc({
    tenant_slug: tenant,
    redirect_uri: redirect.href,
    state,
    code_challenge: challenge,
  });
  const authorization = new URL(result.authorization_url);
  if (
    authorization.protocol !== "https:" &&
    !(
      authorization.protocol === "http:" &&
      ["127.0.0.1", "localhost", "[::1]"].includes(authorization.hostname)
    )
  )
    throw new Error("認証先はHTTPSである必要があります。");
  // These are single-use correlation/PKCE values, never an API or IdP token.
  sessionStorage.setItem(
    storageKey,
    JSON.stringify({
      verifier,
      state,
      created: Date.now(),
      route: location.hash,
    }),
  );
  location.assign(authorization.href);
}

let completion: Promise<{
  api: Api;
  session: Awaited<ReturnType<Api["finishOidc"]>>["session"];
  expires: string;
}> | null = null;

export function completeOidc() {
  if (completion) return completion;
  const fields = new URLSearchParams(location.hash.slice(1));
  if (!fields.has("oidc_code") && !fields.has("oidc_error")) return null;
  // Remove the handoff before loading application data or exposing navigation.
  history.replaceState(null, "", location.pathname + location.search);
  completion = (async () => {
    const raw = sessionStorage.getItem(storageKey);
    sessionStorage.removeItem(storageKey);
    const pending = raw ? JSON.parse(raw) : null;
    if (
      !pending ||
      !pending.state ||
      fields.get("oidc_state") !== pending.state ||
      typeof pending.verifier !== "string" ||
      !Number.isFinite(pending.created) ||
      Date.now() - pending.created > 600_000 ||
      pending.created > Date.now()
    )
      throw new Error(
        "ログインの確認情報が一致しないか、有効期限が切れています。もう一度ログインしてください。",
      );
    if (fields.has("oidc_error"))
      throw new Error(
        "ログインできませんでした。認証をやり直すか、テナントへの所属を管理者に確認してください。",
      );
    const code = fields.get("oidc_code");
    if (!code || !/^[a-f0-9]{64}$/.test(code))
      throw new Error("ログイン応答が不正です。");
    const api = new Api();
    try {
      const result = await api.finishOidc(code, pending.verifier);
      if (
        typeof pending.route === "string" &&
        /^#(?:apps(?:\/[a-zA-Z0-9_-]+)?|deploy|usage)$/.test(pending.route)
      )
        history.replaceState(
          null,
          "",
          location.pathname + location.search + pending.route,
        );
      return { api, ...result };
    } catch (error) {
      api.close();
      throw error;
    }
  })();
  completion.then(
    () => {
      completion = null;
    },
    () => {
      completion = null;
    },
  );
  return completion;
}
