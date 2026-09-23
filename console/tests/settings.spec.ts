import { test, expect, type Page } from "@playwright/test";

async function openSettings(page: Page) {
  await page.goto("/");
  await page.getByLabel("テナント", { exact: true }).fill("team");
  await page.getByRole("button", { name: "組織のアカウントでログイン", exact: true }).click();
  await page.getByRole("link", { name: "hello-api", exact: true }).click();
  await page.getByRole("tab", { name: "設定", exact: true }).click();
}

async function refresh(page: Page) {
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
}

test.beforeEach(async ({ request }) => {
  await request.post("/api/__test/reset", { data: {} });
});

test("settings polling preserves the visible values, text selection and scroll through failures", async ({
  page,
}) => {
  await page.clock.install();
  let hold: Promise<void> | undefined;
  let unavailable = false;
  let stage = "development";
  await page.route("**/api/components/cmp_api/config", async (route) => {
    await hold;
    if (unavailable) return route.fulfill({ status: 503, json: {} });
    const response = await route.fetch();
    const data = await response.json();
    data.env = Object.fromEntries(
      Array.from({ length: 80 }, (_, index) => [
        `VAR_${index}`,
        `setting ${index}`,
      ]),
    );
    data.env.STAGE = stage;
    return route.fulfill({ response, json: data });
  });
  await openSettings(page);
  const settings = page.locator(".settings-sections");
  const cell = page.getByRole("cell", { name: "setting 79", exact: true });
  await cell.scrollIntoViewIfNeeded();
  await cell.evaluate((element) => {
    const range = document.createRange();
    range.selectNodeContents(element);
    const selection = window.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);
  });
  const scroll = await page.evaluate(() => scrollY);
  expect(scroll).toBeGreaterThan(1000);
  let release!: () => void;
  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const refreshing = page.waitForRequest("**/api/components/cmp_api/config");
  await page.clock.fastForward(30_000);
  await refreshing;
  await expect(settings).toBeVisible();
  expect(await page.evaluate(() => scrollY)).toBe(scroll);
  expect(await page.evaluate(() => window.getSelection()?.toString())).toBe(
    "setting 79",
  );
  unavailable = true;
  release();
  hold = undefined;
  await expect(settings.getByRole("alert")).toContainText(
    "表示中の設定は前回取得した内容です。",
  );
  await expect(cell).toBeVisible();
  expect(await page.evaluate(() => window.getSelection()?.toString())).toBe(
    "setting 79",
  );
  await expect(
    page.getByRole("row").filter({ hasText: "API_KEY" }),
  ).toContainText("参照先あり");
  await expect(settings.getByText("1000 ms", { exact: true })).toBeVisible();

  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const retrying = page.waitForRequest("**/api/components/cmp_api/config");
  await refresh(page);
  await retrying;
  // A retry in progress has not made the retained data fresh yet.
  await expect(settings.getByRole("alert")).toContainText("前回取得した内容");
  unavailable = false;
  stage = "production";
  release();
  hold = undefined;
  await expect(
    page.getByRole("cell", { name: "production", exact: true }),
  ).toBeVisible();
  await expect(settings.getByRole("alert")).toHaveCount(0);
});

test("settings from an old version or application cannot replace the current selection", async ({
  page,
  request,
}) => {
  await page.clock.install();
  let holdOld: Promise<void> | undefined;
  let oldCaptured: (() => void) | undefined;
  let failNew = true;
  let mismatch = false;
  await page.route("**/api/components/cmp_api/config", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    if (data.version_id === "ver_2") {
      oldCaptured?.();
      await holdOld;
    } else if (failNew) return route.fulfill({ status: 503, json: {} });
    data.env = { VERSION_MARKER: data.version_id };
    if (mismatch) data.version_id = "unexpected-version";
    return route.fulfill({ response, json: data });
  });
  await openSettings(page);
  const oldValue = page.getByRole("cell", { name: "ver_2", exact: true });
  await expect(oldValue).toBeVisible();
  let release!: () => void;
  holdOld = new Promise<void>((resolve) => {
    release = resolve;
  });
  const pending = new Promise<void>((resolve) => {
    oldCaptured = resolve;
  });
  await refresh(page);
  await pending;
  await request.post("/api/components/cmp_api/rollback", {
    headers: { Authorization: "Bearer cli-fixture-token" },
    data: { version: "1.0.0" },
  });
  await refresh(page);
  await expect(page.getByRole("alert")).toContainText(
    "基盤が処理を受け付けられません",
  );
  await expect(oldValue).toHaveCount(0);
  await expect(page.getByRole("alert")).not.toContainText("前回取得");
  const lateResponse = page.waitForResponse("**/api/components/cmp_api/config");
  release();
  holdOld = undefined;
  await lateResponse;
  await expect(oldValue).toHaveCount(0);
  failNew = false;
  await refresh(page);
  await expect(
    page.getByRole("cell", { name: "ver_1", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);

  mismatch = true;
  await refresh(page);
  await expect(page.getByRole("alert")).toContainText("配備が変更されました");
  await expect(page.getByRole("alert")).toContainText(
    "表示中の設定は前回取得した内容です。",
  );
  await expect(
    page.getByRole("cell", { name: "ver_1", exact: true }),
  ).toBeVisible();
  await page.route("**/api/components/cmp_docs/config", (route) =>
    route.fulfill({ status: 503, json: {} }),
  );
  await page
    .getByRole("link", { name: "アプリケーション", exact: true })
    .click();
  await page.getByRole("link", { name: "docs-preview", exact: true }).click();
  await page.getByRole("tab", { name: "設定", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "基盤が処理を受け付けられません",
  );
  await expect(
    page.getByRole("cell", { name: "ver_1", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByRole("alert")).not.toContainText("前回取得");
  await page.unroute("**/api/components/cmp_docs/config");
  await refresh(page);
  await expect(
    page.getByText("配備されたバージョンはありません。", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("egress refresh retains the draft, caret and focus and separates read and change errors", async ({
  page,
}) => {
  await page.clock.install();
  let hold: Promise<void> | undefined;
  let unavailable = false;
  let rejectChange = false;
  await page.route("**/api/components/cmp_api/egress", async (route) => {
    if (route.request().method() === "PATCH") {
      if (rejectChange) return route.fulfill({ status: 400, json: {} });
      return route.continue();
    }
    await hold;
    if (unavailable) return route.fulfill({ status: 503, json: {} });
    return route.continue();
  });
  await openSettings(page);
  const section = page.locator(".egress-settings");
  const input = page.getByLabel("通信先（ホスト名:ポート）");
  await input.fill("db.example.com:5432");
  await input.evaluate((element: HTMLInputElement) =>
    element.setSelectionRange(3, 10),
  );
  async function expectDraft() {
    await expect(input).toHaveValue("db.example.com:5432");
    await expect(input).toBeFocused();
    expect(
      await input.evaluate((element: HTMLInputElement) => [
        element.selectionStart,
        element.selectionEnd,
      ]),
    ).toEqual([3, 10]);
  }
  let release!: () => void;
  hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  const refreshing = page.waitForRequest("**/api/components/cmp_api/egress");
  await page.clock.fastForward(30_000);
  await refreshing;
  await expectDraft();
  unavailable = true;
  release();
  hold = undefined;
  await expect(section.getByRole("alert")).toContainText(
    "現在の許可状態は確認できていません。",
  );
  await expectDraft();
  unavailable = false;
  await refresh(page);
  await expect(section.getByRole("alert")).toHaveCount(0);
  await expectDraft();

  rejectChange = true;
  await page.getByRole("button", { name: "通信先を許可", exact: true }).click();
  const changeError = section.getByText(
    /URL・認証情報・ワイルドカードは使用できません/,
  );
  await expect(changeError).toBeVisible();
  unavailable = true;
  await refresh(page);
  await expect(section.getByRole("alert")).toHaveCount(2);
  unavailable = false;
  await refresh(page);
  await expect(section.getByRole("alert")).toHaveCount(1);
  await expect(changeError).toBeVisible();
  rejectChange = false;
  await page.getByRole("button", { name: "通信先を許可", exact: true }).click();
  await expect(section.getByRole("alert")).toHaveCount(0);
  await expect(input).toHaveValue("");
  await expect(
    section.getByText("db.example.com:5432", { exact: true }),
  ).toBeVisible();
});

test("initial settings failures show no retained data or assumed egress policy", async ({
  page,
}) => {
  await page.route("**/api/components/cmp_api/config", (route) =>
    route.fulfill({ status: 503, json: {} }),
  );
  await page.route("**/api/components/cmp_api/egress", (route) =>
    route.fulfill({ status: 503, json: {} }),
  );
  await openSettings(page);
  await expect(page.getByRole("alert")).toHaveCount(2);
  await expect(page.getByText(/前回取得した内容/)).toHaveCount(0);
  await expect(
    page.getByRole("heading", { name: "環境変数", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByLabel("通信先（ホスト名:ポート）")).toHaveCount(0);
  await expect(
    page.getByText("なし（外部通信は拒否）"),
  ).toHaveCount(0);
  await page.unroute("**/api/components/cmp_api/config");
  await page.unroute("**/api/components/cmp_api/egress");
  await refresh(page);
  await expect(
    page.getByRole("heading", { name: "環境変数", exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("通信先（ホスト名:ポート）")).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
});
