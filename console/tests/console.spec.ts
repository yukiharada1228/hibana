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
  await page.request.post("/api/__test/identity", { data: { email } });
  await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "アプリケーション", exact: true }),
  ).toBeVisible();
}
test.beforeEach(async ({ request }) => {
  await request.post("/api/__test/reset", { data: {} });
});

test("reload and another tab restore the session without exposing the credential", async ({ page, context }) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  const route = page.url();
  const cookie = (await context.cookies()).find(cookie => cookie.name === "hibana_test_session")!;
  expect(cookie.httpOnly).toBe(true);
  expect(cookie.sameSite).toBe("Strict");
  const requests: string[] = [];
  page.on("request", request => {
    if (request.url().includes("/api/")) requests.push(JSON.stringify(request.headers()));
  });
  await page.reload();
  await expect(page.getByRole("heading", { name: "hello-api", exact: true })).toBeVisible();
  await expect(page).toHaveURL(route);
  expect(await page.evaluate(() => [document.cookie, localStorage.length, sessionStorage.length])).toEqual(["", 0, 0]);
  expect(requests.every(headers => !headers.includes('"authorization"'))).toBe(true);
  expect((await context.cookies()).find(value => value.name === cookie.name)?.expires).toBe(cookie.expires);
  const tab = await context.newPage();
  await tab.goto("/");
  await expect(tab.getByRole("heading", { name: "アプリケーション", exact: true })).toBeVisible();
  await tab.getByRole("button", { name: "ログアウト", exact: true }).click();
  await expect(tab.getByRole("heading", { name: "コンソールにログイン" })).toBeVisible();
  expect((await context.cookies()).some(value => value.name === cookie.name)).toBe(false);
  await page.reload();
  await expect(page.getByRole("heading", { name: "コンソールにログイン" })).toBeVisible();
});

test("restore rejects expired sessions and preserves a session through temporary API failure", async ({ page, context, request }) => {
  await login(page);
  const before = await context.cookies();
  await page.route("**/api/auth/session", route => route.fulfill({ status: 503, json: {} }));
  await page.reload();
  await expect(page.getByRole("alert")).toContainText("基盤が処理を受け付けられません");
  expect(await context.cookies()).toEqual(before);
  await page.unroute("**/api/auth/session");
  await page.reload();
  await expect(page.getByRole("heading", { name: "アプリケーション", exact: true })).toBeVisible();
  await request.post("/api/__test/expire");
  await page.reload();
  await expect(page.getByRole("heading", { name: "コンソールにログイン" })).toBeVisible();
  await expect(page.getByRole("link", { name: "hello-api", exact: true })).toHaveCount(0);
});

test("session restoration tolerates a workstation clock ahead of the server", async ({ page }) => {
  await page.clock.install({ time: new Date(Date.now() + 86_400_000) });
  await login(page);
  await page.reload();
  await expect(page.getByRole("heading", { name: "アプリケーション", exact: true })).toBeVisible();
  await page.clock.fastForward(30_000);
  await expect(page.getByRole("button", { name: "ログアウト", exact: true })).toBeVisible();
});

test("an old tab cannot log out a different session created in another tab", async ({ page, context, browser }) => {
  await login(page);
  const otherContext = await browser.newContext({ baseURL: new URL(page.url()).origin });
  try {
    const otherPage = await otherContext.newPage();
    await login(otherPage, "reader@example.internal");
    const cookie = (await otherContext.cookies()).find(cookie => cookie.name === "hibana_test_session")!;
    await context.addCookies([cookie]);
    const rejected = page.waitForResponse(response => response.url().endsWith("/api/auth/logout") && response.status() === 401);
    await page.getByRole("button", { name: "ログアウト", exact: true }).click();
    await rejected;
    await expect(page.locator(".account-menu summary")).toHaveText("reader@example.internal");
    expect((await context.cookies()).find(value => value.name === cookie.name)?.value).toBe(cookie.value);
    await otherPage.reload();
    await expect(otherPage.getByRole("button", { name: "ログアウト", exact: true })).toBeVisible();
  } finally {
    await otherContext.close();
  }
});

for (const tab of ["バージョン", "拡張"]) {
  test(`${tab} refresh preserves expanded details and focus through failures`, async ({
    page,
  }) => {
    await page.clock.install();
    let revision = 0;
    let fail = "";
    let hold: Promise<void> | undefined;
    await page.route("**/api/components/cmp_api/versions**", async (route) => {
      await hold;
      const list = new URL(route.request().url()).pathname.endsWith(
        "/versions",
      );
      if (fail === (list ? "versions" : "details"))
        return route.fulfill({ status: 503, json: {} });
      const response = await route.fetch();
      const data = await response.json();
      if (list && revision)
        data.push({
          ...data[0],
          version_id: "refresh-version",
          version: "3.0.0",
        });
      else if (!list && data.version_id === "ver_2")
        data.build_metadata.extensions[1].version = `1.2.${3 + revision}`;
      return route.fulfill({ response, json: data });
    });
    await page.route("**/api/components/cmp_api/egress", async (route) => {
      await hold;
      return fail === "egress"
        ? route.fulfill({ status: 503, json: {} })
        : route.fulfill({
            json: { allow_outbound: ["db.example.internal:5432"] },
          });
    });
    await login(page);
    await page.getByRole("link", { name: "hello-api", exact: true }).click();
    await page.getByRole("tab", { name: tab, exact: true }).click();
    const details =
      tab === "バージョン"
        ? page.locator(".version-table .artifact-details").first()
        : page.locator(".extension-dependencies");
    const summary = details.locator("summary");
    await summary.click();
    const destinations = page.locator(".extension-destination-details");
    if (tab === "拡張") {
      await destinations.locator("summary").click();
      await summary.focus();
    }
    async function expectPreserved() {
      await expect(details).toHaveAttribute("open", "");
      await expect(summary).toBeFocused();
      if (tab === "拡張")
        await expect(destinations).toHaveAttribute("open", "");
    }
    let release!: () => void;
    hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    const refreshing = page.waitForRequest(
      "**/api/components/cmp_api/versions",
    );
    await page.clock.fastForward(30_000);
    await refreshing;
    await expectPreserved();
    revision = 1;
    release();
    hold = undefined;
    if (tab === "拡張")
      await expect(page.getByText("1.2.4", { exact: true })).toBeVisible();
    else
      await expect(
        page.getByRole("cell", { name: "3.0.0", exact: false }),
      ).toBeVisible();
    await expectPreserved();

    fail = "versions";
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await expect(page.getByRole("alert")).toContainText(
      "表示中のバージョン一覧は前回取得した内容です。",
    );
    await expectPreserved();
    if (tab === "バージョン") {
      const old = page.getByRole("row").filter({ hasText: "d26b94ca" });
      await old.getByRole("button", { name: "削除", exact: true }).click();
      await expect(
        page
          .getByRole("dialog")
          .getByRole("button", { name: "削除する", exact: true }),
      ).toBeDisabled();
      await page
        .getByRole("button", { name: "キャンセル", exact: true })
        .click();
      await summary.focus();
    } else {
      for (const [failure, message] of [
        ["details", "表示中の拡張構成は前回取得した内容です。"],
        ["egress", "現在の許可状態は確認できていません。"],
      ]) {
        fail = failure;
        await page.evaluate(() => window.dispatchEvent(new Event("focus")));
        await expect(page.getByRole("alert")).toContainText(message);
        await expectPreserved();
      }
    }
    fail = "";
    revision = 2;
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await expect(page.getByRole("alert")).toHaveCount(0);
    if (tab === "拡張")
      await expect(page.getByText("1.2.5", { exact: true })).toBeVisible();
    await expectPreserved();

    if (tab === "拡張") {
      fail = "details";
      await page.getByLabel("確認するバージョン").selectOption("1.0.0");
      await expect(page.getByRole("alert")).toContainText(
        "基盤が処理を受け付けられません",
      );
      await expect(page.getByRole("alert")).not.toContainText("前回取得");
      await expect(details).toHaveCount(0);
      await expect(page.getByText("./database", { exact: true })).toHaveCount(
        0,
      );
      fail = "";
      await page.evaluate(() => window.dispatchEvent(new Event("focus")));
      await expect(
        page.getByText("指定された拡張はありません。", { exact: true }),
      ).toBeVisible();
      await expect(page.getByRole("alert")).toHaveCount(0);
    }
  });
}

test("skip link focuses the current main content without changing its route or form", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await page.getByLabel("確認するバージョン").selectOption("1.0.0");
  const appUrl = page.url();
  async function skip() {
    await page.getByRole("link", { name: "本文へ移動" }).focus();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("main")).toBeFocused();
  }
  await skip();
  await expect(page).toHaveURL(appUrl);
  await expect(
    page.getByRole("tab", { name: "拡張", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(page.getByLabel("確認するバージョン")).toHaveValue("1.0.0");
  await page.keyboard.press("Tab");
  await expect(
    page.getByRole("button", { name: "更新", exact: true }),
  ).toBeFocused();

  await page.getByRole("link", { name: "利用状況", exact: true }).click();
  await page.getByLabel("開始日").fill("2026-09-01");
  const usageUrl = page.url();
  await skip();
  await expect(page).toHaveURL(usageUrl);
  await expect(page.getByLabel("開始日")).toHaveValue("2026-09-01");
});

test("execution refresh preserves open details and focus through updates and errors", async ({
  page,
}) => {
  await page.clock.install();
  let httpStatus = 200;
  let unavailable = false;
  let hold: Promise<void> | undefined;
  await page.route("**/api/components/cmp_api/executions?*", async (route) => {
    await hold;
    if (unavailable) return route.fulfill({ status: 503, json: {} });
    const errorsOnly =
      new URL(route.request().url()).searchParams.get("errors_only") === "true";
    return route.fulfill({
      json: {
        items: errorsOnly
          ? []
          : [
              {
                execution_id: "refresh-execution",
                version_id: "ver_2",
                created_at: "2026-09-18T01:00:00Z",
                wall_time_ms: 12,
                status: "succeeded",
                http_status: httpStatus,
                error: null,
              },
            ],
        next_cursor: null,
      },
    });
  });
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("button", { name: "実行履歴を見る" }).click();
  const details = page.locator(".execution-table details");
  const summary = details.locator("summary");
  await summary.click();
  await expect(summary).toBeFocused();

  let release!: () => void;
  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const refreshing = page.waitForRequest(
    "**/api/components/cmp_api/executions?*",
  );
  await page.clock.fastForward(30_000);
  await refreshing;
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();
  httpStatus = 503;
  release();
  hold = undefined;
  await expect(
    page.getByRole("cell", { name: "HTTP 503", exact: true }),
  ).toBeVisible();
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  unavailable = true;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(page.getByRole("alert")).toContainText(
    "表示中の履歴は前回取得した内容です。",
  );
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  unavailable = false;
  httpStatus = 200;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(
    page.getByRole("cell", { name: "HTTP 200", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  await page
    .getByRole("checkbox", { name: "HTTPエラー・実行失敗のみ" })
    .check();
  await expect(
    page.getByText("該当するエラーはありません", { exact: true }),
  ).toBeVisible();
  await expect(details).toHaveCount(0);
});

test("usage refresh preserves details and focus but clears results for a new range", async ({
  page,
}) => {
  await page.clock.install();
  let invocationCount = 12_408;
  let unavailable = false;
  let hold: Promise<void> | undefined;
  await page.route("**/api/usage?*", async (route) => {
    await hold;
    if (unavailable) return route.fulfill({ status: 503, json: {} });
    const response = await route.fetch();
    const data = await response.json();
    data.totals.invocation_count = invocationCount;
    data.by_component[0].invocation_count = invocationCount;
    return route.fulfill({ response, json: data });
  });
  await login(page);
  await page.getByRole("link", { name: "利用状況", exact: true }).click();
  const details = page.locator("details").filter({ hasText: "リソース使用量" });
  const summary = details.locator("summary");
  await summary.click();
  await expect(summary).toBeFocused();

  let release!: () => void;
  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const refreshing = page.waitForRequest("**/api/usage?*");
  await page.clock.fastForward(30_000);
  await refreshing;
  await expect(
    page.getByRole("cell", { name: "12,408", exact: true }),
  ).toBeVisible();
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();
  invocationCount = 12_409;
  release();
  hold = undefined;
  await expect(
    page.getByRole("cell", { name: "12,409", exact: true }),
  ).toBeVisible();
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  unavailable = true;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(page.getByRole("alert")).toContainText(
    "表示中の利用量は前回取得した内容です。",
  );
  await expect(
    page.getByRole("cell", { name: "12,409", exact: true }),
  ).toBeVisible();
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  unavailable = false;
  invocationCount = 12_410;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(
    page.getByRole("cell", { name: "12,410", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(details).toHaveAttribute("open", "");
  await expect(summary).toBeFocused();

  // Submitting the same dates refreshes without replacing the table.
  invocationCount = 12_411;
  await page.getByRole("button", { name: "集計する" }).click();
  await expect(
    page.getByRole("cell", { name: "12,411", exact: true }),
  ).toBeVisible();
  await expect(details).toHaveAttribute("open", "");

  await page.getByLabel("開始日").fill("2026-09-10");
  await page.getByLabel("終了日").fill("2026-09-09");
  await page.getByRole("button", { name: "集計する" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "開始日は終了日以前を指定してください。",
  );
  await expect(details).toHaveAttribute("open", "");

  // A changed range must not display counts belonging to the previous dates,
  // either while loading or after a failed request.
  await page.getByLabel("開始日").fill("2026-09-01");
  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const newRange = page.waitForRequest(
    "**/api/usage?from=2026-09-01&to=2026-09-09",
  );
  await page.getByRole("button", { name: "集計する" }).click();
  await newRange;
  await expect(
    page.getByText("利用量を読み込み中…", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("12,411", { exact: true })).toHaveCount(0);
  await expect(details).toHaveCount(0);
  unavailable = true;
  release();
  hold = undefined;
  await expect(page.getByRole("alert")).toContainText(
    "基盤が処理を受け付けられません",
  );
  await expect(page.getByRole("alert")).not.toContainText("前回取得");
  await expect(details).toHaveCount(0);

  unavailable = false;
  invocationCount = 7;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(
    page.getByRole("cell", { name: "7", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("2026-09-01 — 2026-09-09 UTC", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(details).not.toHaveAttribute("open", "");
});

test("HTTP errors are distinct from runtime completion and remain readable on mobile", async ({
  page,
}, info) => {
  const base = {
    version_id: "ver_2",
    created_at: "2026-09-18T01:00:00Z",
    wall_time_ms: 12,
    error: null,
  };
  const items = [
    {
      ...base,
      execution_id: "http-error",
      status: "succeeded",
      http_status: 503,
    },
    { ...base, execution_id: "http-ok", status: "succeeded", http_status: 200 },
    {
      ...base,
      execution_id: "runtime-timeout",
      status: "timeout",
      http_status: null,
      error: {
        code: "execution_timeout",
        message: "Execution deadline exceeded",
      },
    },
  ];
  await page.route("**/api/components/cmp_api/executions?*", (route) => {
    const errorsOnly =
      new URL(route.request().url()).searchParams.get("errors_only") === "true";
    return route.fulfill({
      json: {
        items: errorsOnly ? [items[0], items[2]] : items,
        next_cursor: null,
      },
    });
  });
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await expect(page.getByLabel("最新の実行")).toContainText("HTTP 503");
  await page.getByRole("button", { name: "実行履歴を見る" }).click();
  const errors = page.getByRole("checkbox", {
    name: "HTTPエラー・実行失敗のみ",
  });
  await expect(errors).not.toBeChecked();
  await expect(
    page.getByRole("cell", { name: "HTTP 200", exact: true }),
  ).toBeVisible();
  await errors.check();
  await expect(
    page.getByRole("cell", { name: "HTTP 200", exact: true }),
  ).toHaveCount(0);
  const httpError = page.getByRole("row").filter({ hasText: "HTTP 503" });
  await expect(httpError).toContainText("アプリが HTTP エラーを返しました");
  await httpError.getByText("実行の詳細", { exact: true }).click();
  await expect(httpError.locator("details")).toContainText(
    "ランタイム：実行完了",
  );
  await httpError.getByText("実行の詳細", { exact: true }).click();
  await expect(page.locator(".execution-error")).not.toBeVisible();
  await page.screenshot({
    path: info.outputPath("execution-http-desktop.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  const timeout = page.getByRole("cell", {
    name: "結果 タイムアウト",
    exact: true,
  });
  await expect(timeout).toBeVisible();
  expect(
    await timeout
      .locator("strong")
      .evaluate((el) => el.getBoundingClientRect().height),
  ).toBeLessThan(35);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(
    await page
      .locator(".execution-table")
      .evaluate((el) => el.getBoundingClientRect().right <= innerWidth),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("execution-http-mobile.png"),
    fullPage: true,
  });
});

test("execution history keeps its page across polling, focus and manual refresh", async ({
  page,
}) => {
  await page.clock.install();
  await login(page, "reader@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "実行履歴", exact: true }).click();
  await page
    .getByRole("checkbox", { name: "HTTPエラー・実行失敗のみ" })
    .check();
  await page.getByRole("button", { name: "次の20件", exact: true }).click();
  const errors = page
    .locator(".execution-message")
    .filter({ hasText: "Execution deadline exceeded" });
  await expect(errors).toHaveCount(1);
  const queries: URL[] = [];
  page.on("request", (request) => {
    if (request.url().includes("/executions?"))
      queries.push(new URL(request.url()));
  });
  const refreshes = [
    () => page.clock.fastForward(31_000),
    () => page.evaluate(() => window.dispatchEvent(new Event("focus"))),
    () => page.getByRole("button", { name: "更新", exact: true }).click(),
  ];
  for (const refresh of refreshes) {
    queries.length = 0;
    const response = page.waitForResponse((response) =>
      response.url().includes("/executions?"),
    );
    await refresh();
    await response;
    await expect(errors).toHaveCount(1);
    await expect(
      page.getByRole("button", { name: "最新に戻る", exact: true }),
    ).toBeEnabled();
    expect(queries.length).toBeGreaterThan(0);
    expect(queries.every((url) => url.searchParams.has("before"))).toBe(true);
  }
  await page
    .getByRole("checkbox", { name: "HTTPエラー・実行失敗のみ" })
    .uncheck();
  await expect(errors).toHaveCount(20);
  await expect(
    page.getByRole("button", { name: "最新に戻る", exact: true }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "次の20件", exact: true }).click();
  await expect(errors).toHaveCount(1);
  await page
    .getByRole("link", { name: "← アプリケーション", exact: true })
    .click();
  await page.getByRole("link", { name: "docs-preview", exact: true }).click();
  await page.getByRole("tab", { name: "実行履歴", exact: true }).click();
  await expect(errors).toHaveCount(20);
  await expect(
    page.getByRole("button", { name: "最新に戻る", exact: true }),
  ).toBeDisabled();
});

test("administrator manages shared egress with persistence, revocation and visible errors", async ({
  page,
}, info) => {
  await login(page, "admin-only@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "設定", exact: true }).click();
  await expect(page.getByText("なし（外部通信は拒否）", { exact: true })).toBeVisible();
  await page
    .getByLabel("通信先（ホスト名:ポート）")
    .fill("db.example.com:5432");
  await page.getByRole("button", { name: "通信先を許可", exact: true }).click();
  await expect(
    page.getByText("外部通信の許可を更新しました。", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("db.example.com:5432", { exact: true }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("egress-mobile.png"),
    fullPage: true,
  });

  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await page.getByLabel("確認するバージョン").selectOption("1.0.0");
  await expect(page.getByText(/アプリ共通の設定を適用中/)).toBeVisible();
  await expect(
    page.getByText("db.example.com:5432", { exact: true }),
  ).not.toBeVisible();
  await page.getByText("許可された通信先（1件）", { exact: true }).click();
  await expect(
    page.getByText("db.example.com:5432", { exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "通信先の設定を開く", exact: true })
    .click();
  await expect(
    page.getByRole("tab", { name: "設定", exact: true }),
  ).toBeFocused();
  await page.route("**/components/cmp_api/egress", async (route) => {
    if (route.request().method() === "PATCH")
      await route.fulfill({ status: 400, json: {} });
    else await route.continue();
  });
  await page
    .getByLabel("通信先（ホスト名:ポート）")
    .fill("https://wrong.example");
  await page.getByRole("button", { name: "通信先を許可", exact: true }).click();
  await expect(
    page.getByText(/URL・認証情報・ワイルドカードは使用できません/),
  ).toBeVisible();
  await expect(
    page.getByText("外部通信の許可を更新しました。", { exact: true }),
  ).not.toBeVisible();
  await expect(
    page.getByText("db.example.com:5432", { exact: true }),
  ).toBeVisible();
  await page.unroute("**/components/cmp_api/egress");
  await page
    .getByRole("button", {
      name: "db.example.com:5432 の許可を取り消す",
      exact: true,
    })
    .click();
  await expect(
    page.getByText("なし（外部通信は拒否）", { exact: true }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await expect(
    page.getByText("db.example.com:5432", { exact: true }),
  ).not.toBeVisible();
  await expect(
    page.getByText("なし（外部通信は拒否）", { exact: true }),
  ).toBeVisible();
});

test("deployer can inspect egress but cannot change it", async ({ page }) => {
  await login(page, "deployer@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "設定", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "許可された外部通信先", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("通信先の変更には管理者権限が必要です。", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("通信先（ホスト名:ポート）")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "通信先を許可", exact: true }),
  ).toHaveCount(0);
});

test("extension composition distinguishes recorded, empty and unrecorded versions for Read users", async ({
  page,
}, info) => {
  const details: string[] = [];
  page.on("request", (request) => {
    if (/\/versions\/[^/]+$/.test(new URL(request.url()).pathname))
      details.push(request.url());
  });
  await login(page, "reader@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  expect(details).toEqual([]);
  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await expect(page.getByText("./database", { exact: true })).toBeVisible();
  await expect(page.getByText(/アプリ共通の設定を適用中/)).toBeVisible();
  await expect(
    page.getByText("なし（外部通信は拒否）", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("@example/tcp", { exact: true }),
  ).not.toBeVisible();
  await page.getByText("依存する拡張（2）", { exact: true }).click();
  await expect(page.getByText("@example/tls", { exact: true })).toBeVisible();
  await expect(page.getByText("1.2.3", { exact: true })).toBeVisible();
  await expect(
    page.getByText("依存：@example/tcp", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("要求権限：外部通信")).toHaveCount(2);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: info.outputPath("extensions-mobile.png"),
    fullPage: true,
  });
  await page.getByLabel("確認するバージョン").selectOption("1.0.0");
  await expect(
    page.getByText("指定された拡張はありません。", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("なし（外部通信は拒否）", { exact: true }),
  ).toBeVisible();
  await page
    .getByLabel("確認するバージョン")
    .selectOption("0.0.0-dev.1789484656916.d26b94ca");
  await expect(page.getByText(/構成情報が未記録です/)).toBeVisible();
  await expect(
    page.getByText("指定された拡張はありません。", { exact: true }),
  ).toHaveCount(0);
  await page.getByRole("tab", { name: "バージョン", exact: true }).click();
  const old = page.getByRole("row").filter({ hasText: "1.0.0" });
  await old.getByText("バージョンの詳細", { exact: true }).click();
  await old.getByRole("button", { name: "拡張の構成を見る" }).click();
  await expect(page.getByLabel("確認するバージョン")).toHaveValue("1.0.0");
  await expect(
    page.getByText("指定された拡張はありません。", { exact: true }),
  ).toBeVisible();
});

test("extension details fail visibly and do not display another version's metadata", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.route("**/components/cmp_api/versions/by-id/ver_2", (route) =>
    route.fulfill({ status: 503, json: {} }),
  );
  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "基盤が処理を受け付けられません",
  );
  await expect(
    page.getByText("指定された拡張はありません。", { exact: true }),
  ).toHaveCount(0);
  await page.unroute("**/components/cmp_api/versions/by-id/ver_2");
  await page.route(
    "**/components/cmp_api/versions/by-id/ver_2",
    async (route) => {
      const response = await route.fetch();
      await route.fulfill({
        json: { ...(await response.json()), version_id: "wrong-version" },
      });
    },
  );
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "バージョンの情報が一致しません",
  );
});

test("Read users can inspect shared egress and failed policy reads do not imply denied access", async ({
  page,
  request,
}) => {
  await request.patch("/api/components/cmp_api/egress", {
    headers: { Authorization: "Bearer cli-fixture-token" },
    data: { allow: ["shared.example.internal:443"] },
  });
  await login(page, "reader@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.route("**/components/cmp_api/egress", (route) =>
    route.fulfill({ status: 503, json: {} }),
  );
  await page.getByRole("tab", { name: "拡張", exact: true }).click();
  await expect(page.getByText("./database", { exact: true })).toBeVisible();
  await expect(page.getByRole("alert")).toContainText(
    "通信先の適用元を取得できませんでした",
  );
  await expect(
    page.getByText("なし（外部通信は拒否）", { exact: true }),
  ).toHaveCount(0);
  await page.unroute("**/components/cmp_api/egress");
  await page.getByRole("button", { name: "更新", exact: true }).click();
  await expect(page.getByText(/アプリ共通の設定を適用中/)).toBeVisible();
  await page.getByText("許可された通信先（1件）", { exact: true }).click();
  await expect(
    page.getByText("shared.example.internal:443", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("tab", { name: "設定", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "通信先の設定を開く", exact: true }),
  ).toHaveCount(0);
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
    const listed = await run(process.execPath, [cli, "list", "--json"], {
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
      page.getByRole("cell", { name: "バージョン 自動 d26b94ca", exact: true }),
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

test("account details stay inside the viewport when opened on narrow screens", async ({
  page,
}, info) => {
  await login(page);
  const summary = page.locator(".account-menu summary");
  const details = page.locator(".account-menu > div");
  for (const width of [320, 390, 1440]) {
    await page.setViewportSize({ width, height: 844 });
    await summary.focus();
    await page.keyboard.press("Enter");
    await expect(details).toBeVisible();
    await expect(details).toContainText("Development");
    await expect(details).toContainText("閲覧・配備・管理");
    const bounds = await details.boundingBox();
    const viewportWidth = await page.evaluate(
      () => document.documentElement.clientWidth,
    );
    expect(bounds).not.toBeNull();
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewportWidth);
    await page.screenshot({
      path: info.outputPath(`account-menu-${width}.png`),
    });
    await page.keyboard.press("Enter");
    await expect(details).not.toBeVisible();
  }
});

test("CLI guide is available while the initial application list is pending or failed", async ({
  page,
}, info) => {
  let releaseList!: () => void;
  const pendingList = new Promise<void>((resolve) => {
    releaseList = resolve;
  });
  await page.route("**/api/components", async (route) => {
    await pendingList;
    await route.fulfill({ status: 503, json: {} });
  });
  try {
    await page.goto("/#deploy");
    await page.getByLabel("テナント", { exact: true }).fill("team");
    await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
    const guide = page.getByRole("heading", {
      name: "CLI の接続",
      exact: true,
    });
    await expect(guide).toBeVisible();
    await expect(page.locator("pre").first()).toContainText(
      "npx --yes @yukiharada1228/hibana@",
    );
    await expect(page.locator("pre").first()).toContainText("--tenant 'team'");
    await expect(page.locator("pre").last()).toHaveText(
      "npm run deploy",
    );

    releaseList();
    await expect(page.getByRole("alert")).toContainText(
      "基盤が処理を受け付けられません",
    );
    await expect(guide).toBeVisible();
    await expect(
      page.getByRole("button", { name: "コマンドをコピー", exact: true }),
    ).toHaveCount(3);
    await expect(
      page.getByRole("button", { name: "更新", exact: true }),
    ).toHaveCount(0);
    await page.screenshot({
      path: info.outputPath("cli-guide-list-error.png"),
      fullPage: true,
    });

    await page.unroute("**/api/components");
    await page
      .getByRole("link", { name: "アプリケーション", exact: true })
      .click();
    await page.getByRole("button", { name: "更新", exact: true }).click();
    await expect(
      page.getByRole("link", { name: "hello-api", exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("alert")).toHaveCount(0);
  } finally {
    releaseList();
    await page.unroute("**/api/components");
  }
});

test("mobile layout, empty state, command guide and failed login", async ({
  page,
  request,
}, info) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await page.getByLabel("テナント", { exact: true }).fill("team");
  await page.route("**/api/auth/oidc/start", route => route.fulfill({ status: 401, json: {} }));
  await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
  await expect(page.getByRole("alert")).toBeVisible();
  await page.unroute("**/api/auth/oidc/start");
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
  await expect(page.locator("pre").first()).not.toContainText("--email");
  await expect(page.getByText(/ブラウザが開いたら/)).toBeVisible();
  await expect(page.locator("pre").first()).not.toContainText("--profile");
  await expect(page.locator("pre").first()).not.toContainText(
    "--ingress-domain",
  );
  await expect(page.locator("pre").last()).toHaveText(
    "npm run deploy",
  );
  await expect(
    page.getByRole("button", { name: "更新", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByText(/一覧取得/)).toHaveCount(0);
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
  const deleteApp = page.getByRole("button", { name: "アプリを削除", exact: true });
  await deleteApp.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "キャンセル" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(
    dialog.getByRole("button", { name: "削除する", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Shift+Tab");
  await expect(dialog.getByRole("button", { name: "キャンセル" })).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  await expect(deleteApp).toBeFocused();
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
    page
      .locator(".execution-message")
      .filter({ hasText: "Execution deadline exceeded" }),
  ).toHaveCount(20);
  await page.getByRole("button", { name: "次の20件" }).click();
  await expect(
    page
      .locator(".execution-message")
      .filter({ hasText: "Execution deadline exceeded" }),
  ).toHaveCount(1);
  await page.getByRole("button", { name: "最新に戻る" }).click();
  await expect(
    page
      .locator(".execution-message")
      .filter({ hasText: "Execution deadline exceeded" }),
  ).toHaveCount(20);
  await page.getByRole("tab", { name: "設定" }).click();
  await expect(
    page.getByRole("row").filter({ hasText: "API_KEY" }),
  ).toContainText("参照先あり");
  await expect(
    page.getByRole("row").filter({ hasText: "OLD_KEY" }),
  ).toContainText("参照先が削除されています");
  await expect(page.getByText("なし（外部通信は拒否）", { exact: true })).toBeVisible();
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
  expect(requests.some((url) => /\/components\/[^/]+\/config$/.test(url))).toBe(false);
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
  const currentReason = current.getByText(
    "現在のバージョンのため削除できません。",
    { exact: true },
  );
  await expect(currentReason).not.toBeVisible();
  await current.getByText("削除できない理由", { exact: true }).click();
  await expect(currentReason).toBeVisible();
  await current.getByText("削除できない理由", { exact: true }).click();
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
    `http://127.0.0.1:4173/api/components/cmp_api/versions/by-id/ver_old`,
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
  const unselected = page.getByRole("row").filter({ hasText: "0.8.0" });
  await expect(unselected).toContainText("未選択");
  await expect(unselected.getByRole("button", { name: "切り戻す" })).toBeVisible();
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
  const path = `**/components/cmp_api/versions/by-id/ver_old`;
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

for (const name of [".", ".."]) {
  test(`legacy version ${name} can be inspected and deleted by ID`, async ({
    page,
    request,
  }) => {
    await request.post("/api/__test/version-name", {
      data: { id: "ver_old", name },
    });
    await login(page);
    await page.getByRole("link", { name: "hello-api", exact: true }).click();
    await page.getByRole("tab", { name: "拡張", exact: true }).click();
    const details = page.waitForResponse(
      (response) =>
        response.url().endsWith("/versions/by-id/ver_old") &&
        response.request().method() === "GET",
    );
    await page.getByLabel("確認するバージョン").selectOption(name);
    expect((await details).status()).toBe(200);
    await expect(page.getByText(/構成情報が未記録です/)).toBeVisible();
    await page.getByRole("tab", { name: "バージョン", exact: true }).click();
    const row = page
      .getByRole("row")
      .filter({ has: page.getByText(name, { exact: true }) });
    await row.getByRole("button", { name: "削除", exact: true }).click();
    const deleted = page.waitForResponse(
      (response) =>
        response.request().method() === "DELETE" &&
        response.url().endsWith("/versions/by-id/ver_old"),
    );
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "削除する", exact: true })
      .click();
    expect((await deleted).status()).toBe(204);
    await expect(row).toHaveCount(0);
  });
}

test("application logs load on demand, render safely, retry failures and fit mobile", async ({ page }, testInfo) => {
  let mode = "error", requests = 0;
  await page.route("**/api/executions/exe_0", route => {
    requests++;
    if (mode === "error") return route.fulfill({status:503,json:{}});
    return route.fulfill({json:{execution_id:"exe_0",version_id:"ver_2",status:"timeout",logs:
      mode === "expired" ? null : mode === "empty" ? {stdout:"",stderr:"",truncated:false} : {
        stdout:'hello 雪\n<script>window.logExecuted = true</script>\n' + 'long-line-'.repeat(200),
        stderr:"before timeout\n\u001b[2J",truncated:true,
      }}});
  });
  await login(page, "reader@example.internal");
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "実行履歴", exact: true }).click();
  const row = page.getByRole("row").filter({hasText:"exe_0"});
  await row.locator("summary").click();
  expect(requests).toBe(0);
  await row.getByRole("button", {name:"アプリログを表示"}).click();
  await expect(row.getByRole("alert")).toContainText("基盤が処理を受け付けられません");
  mode = "logs";
  await row.getByRole("button", {name:"ログを再取得"}).click();
  await expect(row.getByLabel("標準出力ログ")).toContainText("hello 雪");
  await expect(row.getByLabel("標準出力ログ")).toContainText("<script>");
  await expect(row.getByLabel("標準エラーログ")).toContainText("before timeout");
  await expect(row.getByRole("status")).toContainText("16KiB");
  expect(await page.evaluate(() => (window as unknown as Record<string, unknown>).logExecuted)).toBeUndefined();
  await page.setViewportSize({width:390,height:844});
  await row.getByLabel("標準出力ログ").scrollIntoViewIfNeeded();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({path:testInfo.outputPath("application-logs-mobile.png")});
  mode = "empty";
  await row.getByRole("button", {name:"ログを再取得"}).click();
  await expect(row).toContainText("アプリからの出力はありません");
  mode = "expired";
  await row.getByRole("button", {name:"ログを再取得"}).click();
  await expect(row).toContainText("ログは取得できません");
  await expect(row.getByLabel("標準出力ログ")).toHaveCount(0);
});
