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

test("CLI upload and console rollback share one remote API, without saved browser credentials", async ({
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
      HIBANA_INGRESS_DOMAIN: "apps.example.internal",
      HIBANA_PROFILE: "",
    };
    const cli = resolve("../sdk/src/cli.mjs");
    for (const version of ["1.0.0", "2.0.0"])
      await run(process.execPath, [cli, "deploy", "--version", version], {
        cwd: directory,
        env,
      });
    // CLI processes have exited; the server continues to hold the deployment.
    await page.getByRole("button", { name: "更新", exact: true }).click();
    await expect(
      page.getByRole("link", { name: "cli-app", exact: true }),
    ).toBeVisible();
    await page.screenshot({
      path: info.outputPath("applications.png"),
      fullPage: true,
    });
    await page.getByRole("link", { name: "cli-app", exact: true }).click();
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
    const listed = await run(process.execPath, [cli, "list"], {
      cwd: directory,
      env,
    });
    expect(listed.stdout).toContain("ver_cmp_cli-app_1.0.0");
    await page.screenshot({
      path: info.outputPath("application-detail.png"),
      fullPage: true,
    });
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
  await expect(page.getByRole("button", { name: "アプリを削除" })).toHaveCount(
    0,
  );
  await expect(page.getByRole("tab", { name: "環境変数" })).toHaveCount(0);
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
  await page.getByRole("link", { name: "デプロイ", exact: true }).click();
  await expect(page.locator("pre").first()).toContainText(
    "http://127.0.0.1:4173/api",
  );
  await expect(page.locator("pre").first()).not.toContainText(
    "--password-stdin",
  );
  await expect(page.getByText(/Password: と表示されたら/)).toBeVisible();
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
  await page.getByRole("tab", { name: "環境変数" }).click();
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
