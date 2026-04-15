import { test, expect } from "@playwright/test";

// Showcase mode renders every primary page with fixture data. The daemon is
// running in globalSetup but this project is pointed at the fixture-backed
// Next instance on :3001, so nothing here actually needs the backend.

test.describe("showcase mode (fixtures only)", () => {
  test("dashboard renders from fixtures with showcase badge", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Dashboard", exact: true })).toBeVisible();
    await expect(page.locator("aside").getByText(/^SHOWCASE$/i)).toBeVisible();
    await expect(page.getByTestId("stat-pending-approvals")).toBeVisible();
    await expect(page.getByTestId("stat-wallets-monitored")).toBeVisible();
  });

  test("policy page renders fixture rules", async ({ page }) => {
    await page.goto("/policy");
    await expect(page.getByRole("heading", { name: "Policy", exact: true })).toBeVisible();
    await expect(page.getByText("usdc-high-value-chain").first()).toBeVisible();
  });

  test("audit page renders fixture events", async ({ page }) => {
    await page.goto("/audit");
    await expect(page.getByRole("heading", { name: "Audit trail" })).toBeVisible();
    const body = page.locator("table");
    await expect(body).toContainText("human_approved");
    await expect(body).toContainText("approval_lease_expired");
  });
});
