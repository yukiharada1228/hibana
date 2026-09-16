import { test, expect, type Page } from "@playwright/test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

const run = promisify(execFile);
async function login(page: Page, email = "developer@example.internal") {
  await page.goto("/");
  await page.getByLabel("テナント", { exact: true }).fill("team");
  await page.getByLabel("メールアドレス").fill(email);
  await page.getByLabel("パスワード", { exact: true }).fill("fixture-password");
  await page.getByRole("button", { name: "ログイン", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "アプリケーション", exact: true }),
  ).toBeVisible();
}
test.beforeEach(async ({ request }) => {
  await request.post("/api/__test/reset", { data: {} });
});

test("CLI versions have concise labels while console rollback preserves the full identifier", async ({
  page,
}, info) => {
  await login(page);
  const directory = await mkdtemp(join(tmpdir(), "hibana-console-cli-"));
  try {
    await writeFile(
      join(directory, "app.wasm"),
      Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]),
    );
    await writeFile(
      join(directory, "hibana.json"),
      JSON.stringify({ name: "cli-app", component: "app.wasm" }),
    );
    const env = {
      ...process.env,
      HIBANA_CONFIG_HOME: directory,
      HIBANA_URL: "http://127.0.0.1:4173/api",
      HIBANA_TOKEN: "cli-fixture-token",
      HIBANA_TENANT: "team",
      HIBANA_PROFILE: "",
    };
    const cli = resolve("../sdk/src/cli.mjs");
    const automatic = "0.0.0-dev.1789484656916.d26b94ca";
    const latest = "0.0.0-dev.1789484657916.ef012345";
    for (const version of ["1.0.0", automatic, latest])
      await run(process.execPath, [cli, "deploy", "--version", version], {
        cwd: directory,
        env,
      });
    // CLI processes have exited; the server continues to hold the deployment.
    await page.getByRole("button", { name: "更新", exact: true }).click();
    await expect(
      page.getByRole("link", { name: "cli-app", exact: true }),
    ).toBeVisible();
    await expect(
      page.getByText("自動 ef012345", { exact: true }),
    ).toBeVisible();
    await expect(page.getByText(latest, { exact: true })).not.toBeVisible();
    await page.screenshot({
      path: info.outputPath("applications.png"),
      fullPage: true,
    });
    await page.getByRole("link", { name: "cli-app", exact: true }).click();
    await expect(
      page.getByRole("row").filter({ hasText: "1.0.0" }),
    ).toBeVisible();
    const previous = page.getByRole("row").filter({ hasText: automatic });
    await expect(
      previous.getByText("自動 d26b94ca", { exact: true }),
    ).toBeVisible();
    await expect(
      previous.getByText(automatic, { exact: true }),
    ).not.toBeVisible();
    await previous.getByText("バージョンの詳細", { exact: true }).click();
    await expect(previous.getByText(automatic, { exact: true })).toBeVisible();
    await previous.getByRole("button", { name: "切り戻す" }).click();
    await expect(
      page.getByRole("dialog").getByText(automatic, { exact: true }),
    ).toBeVisible();
    const rollbackRequest = page.waitForRequest(
      (request) =>
        request.method() === "POST" && request.url().endsWith("/rollback"),
    );
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "切り戻す", exact: true })
      .click();
    expect((await rollbackRequest).postDataJSON()).toEqual({
      version: automatic,
    });
    await expect(previous).toContainText("現在のバージョン");
    const listed = await run(process.execPath, [cli, "list"], {
      cwd: directory,
      env,
    });
    expect(listed.stdout).toContain(`ver_cmp_cli-app_${automatic}`);
    await page.screenshot({
      path: info.outputPath("application-detail.png"),
      fullPage: true,
    });
    await page.setViewportSize({ width: 390, height: 844 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await page.screenshot({
      path: info.outputPath("automatic-version-mobile.png"),
      fullPage: true,
    });
    await page.getByRole("tab", { name: "実行履歴", exact: true }).click();
    await expect(
      page.getByRole("cell", { name: "自動 d26b94ca", exact: true }),
    ).toHaveCount(20);
    await page.getByText("実行の詳細", { exact: true }).first().click();
    await expect(
      page.locator("details[open]").getByText(automatic, { exact: true }),
    ).toBeVisible();
    expect(
      await page.evaluate(() => [
        localStorage.length,
        sessionStorage.length,
        document.cookie,
      ]),
    ).toEqual([0, 0, ""]);
    await page.getByRole("button", { name: "ログアウト" }).click();
    await expect(
      page.getByRole("heading", { name: "コンソールにログイン" }),
    ).toBeVisible();
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("read-only account, usage filters, API errors and session expiration", async ({
  page,
  request,
}) => {
  await login(page, "reader@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await expect(
    page.getByRole("cell", { name: "1.0.0", exact: false }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "切り戻す" })).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "削除", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByRole("button", { name: "アプリを削除" })).toHaveCount(
    0,
  );
  await expect(page.getByRole("tab", { name: "設定" })).toHaveCount(0);
  await page.getByRole("link", { name: "利用状況", exact: true }).click();
  await expect(page.getByText("12,408").first()).toBeVisible();
  await page.getByLabel("開始日").fill("2026-09-10");
  await page.getByLabel("終了日").fill("2026-09-09");
  await page.getByRole("button", { name: "集計する" }).click();
  await expect(page.getByRole("alert")).toContainText("開始日");
  await request.post("/api/__test/unavailable");
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(page.getByRole("alert").first()).toContainText(
    "基盤が処理を受け付けられません",
  );
  await request.post("/api/__test/expire");
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "コンソールにログイン" }),
  ).toBeVisible();
});

test("mobile layout, empty state, command guide and failed login", async ({
  page,
  request,
}, info) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await page.getByLabel("テナント", { exact: true }).fill("team");
  await page.getByLabel("メールアドレス").fill("developer@example.internal");
  await page.getByLabel("パスワード", { exact: true }).fill("wrong");
  await page.getByRole("button", { name: "ログイン", exact: true }).click();
  await expect(page.getByRole("alert")).toBeVisible();
  await login(page);
  await expect(
    page.getByRole("link", { name: "hello-api", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("mobile-applications.png"),
    fullPage: true,
  });
  await request.post("/api/__test/reset", { data: { empty: true } });
  await login(page);
  await expect(
    page.getByRole("heading", { name: "最初のアプリをデプロイしましょう" }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("mobile-empty.png"),
    fullPage: true,
  });
  await page
    .getByRole("navigation")
    .getByRole("link", { name: "CLI の接続", exact: true })
    .click();
  await expect(page.locator("pre").first()).toContainText(
    "http://127.0.0.1:4173/api",
  );
  await expect(page.locator("pre").first()).not.toContainText(
    "--password-stdin",
  );
  await expect(page.getByText(/Password: と表示されたら/)).toBeVisible();
  await expect(page.locator("pre").first()).not.toContainText("--profile");
  await expect(page.locator("pre").first()).not.toContainText(
    "--ingress-domain",
  );
  await expect(page.locator("pre").last()).toHaveText("hibana deploy");
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

test("environment values and deletion require an explicit confirmation", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "設定" }).click();
  await expect(
    page.getByRole("cell", { name: "Hello from Hibana", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "アプリを削除", exact: true }).click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "キャンセル" })
    .click();
  await expect(
    page.getByRole("heading", { name: "hello-api", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "アプリを削除", exact: true }).click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "削除する", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "アプリケーション", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "hello-api", exact: true }),
  ).toHaveCount(0);
});

test("operational details, pagination and usage refresh reflect the platform", async ({
  page,
  request,
}, info) => {
  await login(page);
  await expect(
    page.getByRole("link", { name: "hello-api を開く", exact: true }),
  ).toHaveAttribute(
    "href",
    "https://hello-api.team.apps.example.internal:8443/",
  );
  await expect(page.getByText("イントラネット接続")).toHaveCount(0);
  await expect(page.getByText("実行環境", { exact: true })).toHaveCount(0);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await expect(page.getByText("公開設定済み", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "実行履歴" }).click();
  await expect(
    page.getByText("Execution deadline exceeded", { exact: false }),
  ).toHaveCount(20);
  await page.getByRole("button", { name: "次の20件" }).click();
  await expect(
    page.getByText("Execution deadline exceeded", { exact: false }),
  ).toHaveCount(1);
  await page.getByRole("button", { name: "最新に戻る" }).click();
  await expect(
    page.getByText("Execution deadline exceeded", { exact: false }),
  ).toHaveCount(20);
  await page.getByRole("tab", { name: "設定" }).click();
  await expect(
    page.getByRole("row").filter({ hasText: "API_KEY" }),
  ).toContainText("参照先あり");
  await expect(
    page.getByRole("row").filter({ hasText: "OLD_KEY" }),
  ).toContainText("参照先が削除されています");
  await expect(
    page.getByText("db.example.internal:5432", { exact: true }),
  ).toBeVisible();
  await page.screenshot({
    path: info.outputPath("settings.png"),
    fullPage: true,
  });
  await page.getByRole("link", { name: "利用状況", exact: true }).click();
  await expect(page.getByText("12,408").first()).toBeVisible();
  await request.post("/api/__test/activity");
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(page.getByText("12,409").first()).toBeVisible();
});

test("administrators without Deploy can delete without fetching plaintext configuration", async ({
  page,
}) => {
  const requests: string[] = [];
  page.on("request", (request) => requests.push(request.url()));
  await login(page, "admin-only@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "設定" }).click();
  await expect(
    page.getByRole("button", { name: "アプリを削除", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "環境変数", exact: true }),
  ).toHaveCount(0);
  expect(requests.some((url) => url.endsWith("/config"))).toBe(false);
});

test("version deletion protects live versions, confirms the full identifier and preserves the app", async ({
  page,
  request,
}, info) => {
  await login(page, "admin-only@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  const current = page.getByRole("row").filter({ hasText: "2.0.0" });
  const previous = page.getByRole("row").filter({ hasText: "1.0.0" });
  const executing = page.getByRole("row").filter({ hasText: "0.8.0" });
  for (const row of [current, previous, executing])
    await expect(
      row.getByRole("button", { name: "削除", exact: true }),
    ).toBeDisabled();
  await expect(current).toContainText("現在のバージョンのため削除できません");
  await expect(previous).toContainText("切り戻し先として保護されています");
  await expect(executing).toContainText("実行中・実行待ちの処理");
  await expect(page.getByRole("button", { name: "切り戻す" })).toHaveCount(0);

  const version = "0.0.0-dev.1789484656916.d26b94ca";
  const old = page.getByRole("row").filter({ hasText: version });
  const deletes: string[] = [];
  page.on("request", (r) => {
    if (r.method() === "DELETE") deletes.push(r.url());
  });
  await old.getByRole("button", { name: "削除", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText(version, { exact: true })).toBeVisible();
  await expect(dialog).toContainText("この版へ切り戻せなくなります");
  await dialog.getByRole("button", { name: "キャンセル" }).click();
  expect(deletes).toEqual([]);
  await expect(old).toBeVisible();

  await old.getByRole("button", { name: "削除", exact: true }).click();
  await page.screenshot({
    path: info.outputPath("delete-version-confirm.png"),
    fullPage: true,
  });
  await dialog.getByRole("button", { name: "削除する", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(old).toHaveCount(0);
  await expect(page.getByRole("status")).toContainText(
    "バージョン 自動 d26b94ca を削除しました",
  );
  expect(deletes).toEqual([
    `http://127.0.0.1:4173/api/components/cmp_api/versions/${version}`,
  ]);
  await expect(current).toContainText("現在のバージョン");
  await expect(
    page.getByRole("link", { name: "hello-api を開く", exact: true }),
  ).toBeVisible();
  const stored = await request.get("/api/components/cmp_api/versions", {
    headers: { Authorization: "Bearer cli-fixture-token" },
  });
  expect(
    (await stored.json()).some(
      (item: { version: string }) => item.version === version,
    ),
  ).toBe(false);
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(old).toHaveCount(0);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("version-protection-mobile.png"),
    fullPage: true,
  });
  await executing
    .getByRole("button", { name: "削除", exact: true })
    .scrollIntoViewIfNeeded();
  await page.screenshot({
    path: info.outputPath("version-protection-actions-mobile.png"),
    fullPage: true,
  });
});

test("Deploy without Admin can roll back but cannot delete versions", async ({
  page,
}) => {
  await login(page, "deployer@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await expect(
    page
      .getByRole("row")
      .filter({ hasText: "1.0.0" })
      .getByRole("button", { name: "切り戻す" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "削除", exact: true }),
  ).toHaveCount(0);
});

test("deletion conflict refreshes protection and cancellation keeps the version list usable", async ({
  page,
  request,
}) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  const version = "0.0.0-dev.1789484656916.d26b94ca";
  const old = page.getByRole("row").filter({ hasText: version });
  await old.getByRole("button", { name: "削除", exact: true }).click();
  // Another operator selects the version while the confirmation is open.
  await request.post("/api/components/cmp_api/rollback", {
    headers: { Authorization: "Bearer cli-fixture-token" },
    data: { version },
  });
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("button", { name: "削除する", exact: true }).click();
  await expect(dialog.getByRole("alert")).toContainText(
    "現在の状態では操作できません",
  );
  await expect(dialog).toContainText("現在のバージョンのため削除できません");
  await expect(
    dialog.getByRole("button", { name: "削除する", exact: true }),
  ).toBeDisabled();
  await dialog.getByRole("button", { name: "キャンセル" }).click();
  await expect(old).toContainText("現在のバージョン");
  await expect(
    old.getByRole("button", { name: "削除", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("row").filter({ hasText: "2.0.0" }),
  ).toContainText("切り戻し先");
});

test("a failed deletion keeps the version and can be retried", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  const version = "0.0.0-dev.1789484656916.d26b94ca";
  const path = `**/components/cmp_api/versions/${version}`;
  await page.route(path, (route) => route.fulfill({ status: 503, json: {} }));
  const old = page.getByRole("row").filter({ hasText: version });
  await old.getByRole("button", { name: "削除", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("button", { name: "削除する", exact: true }).click();
  await expect(dialog.getByRole("alert")).toContainText(
    "基盤が処理を受け付けられません",
  );
  await expect(old).toBeVisible();
  await page.unroute(path);
  await dialog.getByRole("button", { name: "削除する", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(old).toHaveCount(0);
});
