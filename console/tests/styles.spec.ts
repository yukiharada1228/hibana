import { test, expect } from "@playwright/test";
import { readFileSync } from "node:fs";
import { cn } from "../src/lib/digital-agency/cn";

test("every Digital Agency font size keeps its independent text color", () => {
  const css = readFileSync(
    new URL("../src/styles/digital-agency.css", import.meta.url),
    "utf8",
  );
  const sizes = [...css.matchAll(/--text-([^\s:]+):/g)]
    .map((match) => match[1])
    .filter((name) => !name.includes("--"));
  expect(sizes.length).toBeGreaterThan(0);
  for (const size of sizes) {
    const font = `text-${size}`;
    expect(cn("text-key-900", font)).toBe(`text-key-900 ${font}`);
    expect(cn(font, "text-key-900")).toBe(`${font} text-key-900`);
    expect(cn("text-sm", font)).toBe(font);
    expect(cn(font, "text-sm")).toBe("text-sm");
  }
});

test("custom scales override defaults, including directional corners", () => {
  expect(cn("rounded-full", "rounded-8")).toBe("rounded-8");
  expect(cn("rounded-t-lg", "rounded-t-8")).toBe("rounded-t-8");
  expect(cn("rounded-ss-lg", "rounded-ss-8")).toBe("rounded-ss-8");
  expect(cn("shadow-lg", "shadow-3", "shadow-key-900")).toBe(
    "shadow-3 shadow-key-900",
  );
  expect(cn("leading-normal", "leading-150", "leading-1-75")).toBe(
    "leading-1-75",
  );
  expect(cn("p-2!", [false, "p-4!"], { "p-6!": true })).toBe("p-6!");
});
