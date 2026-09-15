import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  testMatch: "*.spec.ts",
  workers: 1,
  use: {
    baseURL: "http://127.0.0.1:4173",
    viewport: { width: 1440, height: 1000 },
    trace: "retain-on-failure",
  },
  webServer: [
    {
      command: "node tests/mock-api.mjs",
      url: "http://127.0.0.1:4191/healthz",
      reuseExistingServer: false,
    },
    {
      command: "npm run dev -- --port 4173",
      url: "http://127.0.0.1:4173",
      env: { HIBANA_API_UPSTREAM: "http://127.0.0.1:4191" },
      reuseExistingServer: false,
    },
  ],
});
