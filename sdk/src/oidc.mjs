import { createServer } from "node:http";
import { randomBytes, createHash } from "node:crypto";
import { spawn } from "node:child_process";

async function openBrowser(url) {
  const [command, args] =
    process.platform === "darwin"
      ? ["open", [url]]
      : process.platform === "win32"
        ? ["rundll32", ["url.dll,FileProtocolHandler", url]]
        : ["xdg-open", [url]];
  await new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      shell: false,
      stdio: "ignore",
      detached: true,
    });
    child.once("error", reject);
    child.once("spawn", () => {
      child.unref();
      resolve();
    });
  });
}

/** Browser authorization uses a one-shot loopback callback, never a password. */
export async function browserLogin(
  api,
  {
    noBrowser = false,
    open = openBrowser,
    timeoutMs = 600_000,
    signal,
    log = console.log,
  } = {},
) {
  if (!api.tenant) throw new Error("Specify --tenant TEAM (or HIBANA_TENANT)");
  const verifier = randomBytes(32).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  const state = randomBytes(32).toString("base64url");
  const lifetime = new AbortController();
  const cancel = () => lifetime.abort(new Error("Login cancelled"));
  const timer = setTimeout(
    () => lifetime.abort(new Error("Login timed out; run hibana login again")),
    timeoutMs,
  );
  process.on("SIGINT", cancel);
  process.on("SIGTERM", cancel);
  signal?.addEventListener("abort", cancel, { once: true });
  if (signal?.aborted) cancel();
  let resolveCode,
    rejectCode,
    received = false;
  const code = new Promise((resolve, reject) => {
    resolveCode = resolve;
    rejectCode = reject;
  });
  code.catch(() => {});
  lifetime.signal.addEventListener(
    "abort",
    () => rejectCode(lifetime.signal.reason),
    { once: true },
  );
  const server = createServer(
    { maxHeaderSize: 8192, requestTimeout: 10_000, headersTimeout: 10_000 },
    (request, response) => {
      response.setHeader("Cache-Control", "no-store");
      response.setHeader("Referrer-Policy", "no-referrer");
      response.setHeader("Content-Type", "text/plain; charset=utf-8");
      response.setHeader(
        "Content-Security-Policy",
        "default-src 'none'; frame-ancestors 'none'",
      );
      let url;
      try {
        url = new URL(request.url, "http://127.0.0.1");
      } catch {
        response.writeHead(400).end("Invalid login callback.");
        return;
      }
      if (
        received ||
        request.method !== "GET" ||
        request.headers.host !== `127.0.0.1:${server.address().port}` ||
        url.pathname !== "/oidc/callback" ||
        url.searchParams.get("oidc_state") !== state
      ) {
        response.writeHead(400).end("Invalid login callback.");
        return;
      }
      const value = url.searchParams.get("oidc_code");
      if (
        !url.searchParams.has("oidc_error") &&
        !/^[a-f0-9]{64}$/.test(value || "")
      ) {
        response.writeHead(400).end("Invalid login callback.");
        return;
      }
      received = true;
      if (url.searchParams.has("oidc_error")) {
        response
          .writeHead(401)
          .end("Login failed. Return to the terminal and try again.");
        rejectCode(
          new Error(
            "Login failed. Check your identity and tenant membership with the administrator.",
          ),
        );
      } else {
        response.end(
          "Browser authentication received. Return to the terminal to check that login completed.",
        );
        resolveCode(value);
      }
    },
  );
  try {
    lifetime.signal.throwIfAborted();
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    const started = await api.request("/auth/oidc/start", {
      method: "POST",
      auth: false,
      signal: lifetime.signal,
      body: {
        tenant_slug: api.tenant,
        redirect_uri: `http://127.0.0.1:${server.address().port}/oidc/callback`,
        code_challenge: challenge,
        state,
      },
    });
    const authorization = new URL(started.authorization_url);
    if (
      authorization.protocol !== "https:" &&
      !(
        authorization.protocol === "http:" &&
        ["127.0.0.1", "localhost", "[::1]"].includes(authorization.hostname)
      )
    )
      throw new Error("Identity provider must use HTTPS");
    if (authorization.username || authorization.password)
      throw new Error("Invalid identity provider URL");
    log(`Open this URL in a browser on this computer:\n${authorization.href}`);
    if (!noBrowser)
      await open(authorization.href).catch(() =>
        log("Could not open a browser automatically; open the URL above."),
      );
    return await api.request("/auth/oidc/exchange", {
      method: "POST",
      auth: false,
      signal: lifetime.signal,
      body: { code: await code, code_verifier: verifier },
    });
  } finally {
    clearTimeout(timer);
    process.removeListener("SIGINT", cancel);
    process.removeListener("SIGTERM", cancel);
    signal?.removeEventListener("abort", cancel);
    server.closeAllConnections();
    if (server.listening) await new Promise((resolve) => server.close(resolve));
  }
}
