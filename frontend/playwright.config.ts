import { defineConfig, devices } from "@playwright/test";

// Phase 3 E2E config. We intentionally do NOT use Playwright's `webServer`
// because daemon + Next dev servers must start in a strict order (daemon
// health → seed → next dev × 2). globalSetup owns the whole lifecycle.

export default defineConfig({
  testDir: "./tests/e2e",
  timeout: 30_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  workers: 1,
  // Durable artifacts land in frontend/playwright-report/. The list reporter
  // keeps stdout tight during debug loops; the html + json reporters give
  // reviewers / CI something to diff or upload as an artifact later on.
  reporter: [
    ["list"],
    ["html", { outputFolder: "playwright-report", open: "never" }],
    ["json", { outputFile: "playwright-report/results.json" }],
  ],
  retries: 0,

  globalSetup: require.resolve("./tests/e2e/global-setup.ts"),
  globalTeardown: require.resolve("./tests/e2e/global-teardown.ts"),

  projects: [
    {
      name: "showcase",
      testMatch: /.*\.showcase\.spec\.ts/,
      use: {
        ...devices["Desktop Chrome"],
        baseURL: "http://127.0.0.1:3001",
      },
    },
    {
      name: "live",
      testMatch: /.*\.live\.spec\.ts/,
      use: {
        ...devices["Desktop Chrome"],
        baseURL: "http://127.0.0.1:3000",
      },
    },
  ],
});
