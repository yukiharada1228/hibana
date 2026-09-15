import { expect, test } from "@playwright/test";
import { appUrl } from "../src/api";
import type { Component, Session } from "../src/types";

test("application links respect the server domain and configured origin", () => {
  const component = {
    name: "hello",
    ingress_enabled: true,
    active_version_id: "version-1",
  } as Component;
  const session = {
    tenant_slug: "team",
    ingress_base_domain: "apps.example.com",
  } as Session;
  expect(appUrl(component, session, "")).toBe(
    "https://hello.team.apps.example.com/",
  );
  expect(appUrl(component, session, "https://apps.example.com:8443")).toBe(
    "https://hello.team.apps.example.com:8443/",
  );
  for (const origin of [
    "http://apps.example.com",
    "https://other.example.com",
    "invalid",
    "https://user:password@apps.example.com",
    "https://apps.example.com/path",
    "https://apps.example.com/?token=x",
    "https://apps.example.com/#fragment",
  ])
    expect(appUrl(component, session, origin)).toBeNull();
  expect(
    appUrl(
      component,
      { ...session, ingress_base_domain: "localhost" },
      "http://localhost:28084",
    ),
  ).toBe("http://hello.team.localhost:28084/");
  expect(
    appUrl({ ...component, ingress_enabled: false }, session, ""),
  ).toBeNull();
  expect(
    appUrl({ ...component, name: "invalid/name" }, session, ""),
  ).toBeNull();
});
