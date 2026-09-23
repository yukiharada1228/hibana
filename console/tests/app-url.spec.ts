import { expect, test } from "@playwright/test";
import { appUrl, publication } from "../src/api";
import type { Component } from "../src/types";

test("application URLs come from the platform, with safe navigation and truthful publication states", () => {
  const component = {
    ingress_enabled: true,
    active_version_id: "version-1",
  } as Component;
  for (const url of [
    "https://hello.team.apps.example.com/",
    "https://hello.team.apps.example.com:8443/",
    "http://hello.team.localhost:28084/",
  ]) {
    expect(appUrl({ ...component, public_url: url })).toBe(url);
  }
  for (const url of [
    "invalid",
    "javascript:alert(1)",
    "http://hello.team.apps.example.com/",
    "https://user:password@example.com/",
    "https://example.com/?token=x",
  ]) {
    expect(appUrl({ ...component, public_url: url })).toBeNull();
  }
  expect(publication({ ...component, public_url: null })).toBe(
    "公開 URL 未設定",
  );
  expect(publication({ ...component, ingress_enabled: false })).toBe("非公開");
  expect(publication({ ...component, active_version_id: null })).toBe("未配備");
});
