// Runs inside test-http.sh's disposable platform; never connects to a configured site.
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { randomUUID } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";
import { runCommand } from "./bounded-process.mjs";

export async function testConsole({ api, token, url, app, wasm, folder }) {
  const session = await (await api("/auth/session", { token })).json();
  assert.equal(session.tenant_slug, "upload");
  assert.equal(session.ingress_base_domain, "hibana.test");
  assert.equal((await api("/auth/session")).status, 401);
  assert.equal((await api("/auth/logout", { method: "POST" })).status, 401);
  const reader = await (
    await api("/auth/login", {
      method: "POST",
      body: {
        tenant_slug: "upload",
        email: "test@example.invalid",
        password: "test-password",
        scopes: ["read"],
      },
    })
  ).json();
  assert.equal(
    (await api("/auth/session", { token: reader.token })).status,
    200,
  );
  assert.equal(
    (
      await api("/auth/logout", {
        token: reader.token,
        method: "POST",
        body: { token_id: "another-token" },
      })
    ).status,
    204,
  );
  assert.equal(
    (await api("/components", { token: reader.token })).status,
    401,
    "Read token can revoke itself",
  );
  assert.equal(
    (await api("/auth/session", { token })).status,
    200,
    "logout cannot revoke another token",
  );
  const list = await (await api("/components", { token })).json();
  assert.ok(list.every((c) => typeof c.ingress_enabled === "boolean"));
  for (const c of list.filter(c => c.ingress_enabled && c.active_version_id)) {
    assert.equal(c.public_url, `https://${c.name}.upload.hibana.test/`);
    assert.ok(c.active_version);
    assert.ok(c.active_version_created_at);
  }
  console.log(
    "PASS console session uses authenticated tenant and Read users revoke only their own token",
  );

  const image = process.env.HIBANA_TEST_CONSOLE_IMAGE;
  if (!image) return;
  const { chromium } = await import(
    "../console/node_modules/playwright/index.mjs"
  );
  const { expect } = await import(
    "../console/node_modules/@playwright/test/index.mjs"
  );
  const name = `hibana-console-probe-${randomUUID()}`;
  const project = join(folder, "console-project");
  await mkdir(project);
  await writeFile(join(project, "app.wasm"), wasm);
  const upstream =
    process.platform === "linux"
      ? url
      : url.replace("127.0.0.1", "host.docker.internal");
  const network =
    process.platform === "linux"
      ? ["--network", "host"]
      : ["-p", "127.0.0.1::8080"];
  let browser;
  try {
    await runCommand("docker", [
      "run",
      "--rm",
      "-d",
      "--name",
      name,
      "--read-only",
      "--tmpfs",
      "/tmp:rw,noexec,nosuid,size=64m,uid=101,gid=101",
      "--cap-drop",
      "ALL",
      "--security-opt",
      "no-new-privileges",
      ...network,
      "-e",
      `HIBANA_API_UPSTREAM=${upstream}`,
      image,
    ]);
    const port =
      process.platform === "linux"
        ? "8080"
        : (await runCommand("docker", ["port", name, "8080/tcp"]))
            .trim()
            .split(":")
            .at(-1);
    const consoleUrl = `http://127.0.0.1:${port}`;
    for (let attempt = 0; ; attempt++) {
      try {
        if (
          (
            await fetch(`${consoleUrl}/healthz`, {
              signal: AbortSignal.timeout(500),
            })
          ).ok
        )
          break;
      } catch {}
      if (attempt === 50) throw new Error("Console image did not start");
      await sleep(100);
    }
    const html = await fetch(consoleUrl);
    assert.equal(html.status, 200);
    assert.match(
      html.headers.get("content-security-policy"),
      /frame-ancestors 'none'/,
    );
    assert.equal(html.headers.get("cache-control"), "no-store");
    assert.equal((await fetch(`${consoleUrl}/api/auth/session`)).status, 401);
    assert.equal((await fetch(`${consoleUrl}/assets/missing.js`)).status, 404);
    const env = {
      ...process.env,
      HIBANA_URL: `${consoleUrl}/api`,
      HIBANA_TOKEN: token,
      HIBANA_TENANT: "upload",
      HIBANA_PROFILE: "",
      HIBANA_CONFIG_HOME: project,
    };
    for (const version of ["1.0.0", "2.0.0"]) {
      await writeFile(
        join(project, "hibana.json"),
        JSON.stringify({
          name: "console-app",
          component: "app.wasm",
          vars: { GREETING: version },
        }),
      );
      await runCommand(
        process.execPath,
        [resolve("sdk/src/cli.mjs"), "deploy", "--version", version],
        { cwd: project, env, timeoutMs: 120000 },
      );
    }
    assert.equal(
      (await app("console-app")).body.message,
      "2.0.0",
      "app serves after both CLI processes exit",
    );
    browser = await chromium.launch();
    const page = await browser.newPage({
      viewport: { width: 1440, height: 1000 },
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(consoleUrl);
    await page.getByLabel("テナント", { exact: true }).fill("upload");
    await page.getByLabel("メールアドレス").fill("test@example.invalid");
    await page.getByLabel("パスワード", { exact: true }).fill("test-password");
    await page.getByRole("button", { name: "ログイン", exact: true }).click();
    await page.getByRole("link", { name: "console-app", exact: true }).click();
    await expect(
      page.getByRole("row").filter({ hasText: "2.0.0" }),
    ).toContainText("現在のバージョン");
    await page
      .getByRole("row")
      .filter({ hasText: "1.0.0" })
      .getByRole("button", { name: "切り戻す" })
      .click();
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "切り戻す", exact: true })
      .click();
    await expect(
      page.getByRole("row").filter({ hasText: "1.0.0" }),
    ).toContainText("現在のバージョン");
    assert.equal(
      (await app("console-app")).body.message,
      "1.0.0",
      "browser rollback changes real Wasmtime HTTP response",
    );
    const listed = JSON.parse(
      await runCommand(process.execPath, [resolve("sdk/src/cli.mjs"), "list"], {
        cwd: project,
        env,
      }),
    );
    const component = listed.find((c) => c.name === "console-app");
    const versions = await (
      await api(`/components/${component.component_id}/versions`, { token })
    ).json();
    assert.equal(
      component.active_version_id,
      versions.find((v) => v.version === "1.0.0").version_id,
    );
    await mkdir(".local/verification/console", { recursive: true });
    await page.screenshot({
      path: ".local/verification/console/real-platform.png",
      fullPage: true,
    });
    await page.getByRole("button", { name: "ログアウト" }).click();
    await expect(
      page.getByRole("heading", { name: "コンソールにログイン" }),
    ).toBeVisible();
    assert.equal(
      (await app("console-app")).status,
      200,
      "logout does not stop the application",
    );
    assert.deepEqual(errors, []);
    await writeFile(
      ".local/verification/console/report.json",
      JSON.stringify(
        {
          passed: true,
          checks: [
            "readonly production nginx image",
            "same-origin API proxy",
            "CLI deployment after process exit",
            "real PostgreSQL and Wasmtime",
            "browser rollback changes HTTP response",
            "CLI sees browser changes",
            "logout preserves application",
          ],
        },
        null,
        2,
      ) + "\n",
    );
    assert.equal(
      (
        await api(`/components/${component.component_id}`, {
          token,
          method: "DELETE",
        })
      ).status,
      204,
    );
    console.log(
      "PASS production console image + real API: CLI deploy → Wasmtime HTTP → browser rollback → CLI readback → logout",
    );
  } finally {
    await browser?.close();
    await runCommand("docker", ["stop", name]).catch(() => {});
  }
}
