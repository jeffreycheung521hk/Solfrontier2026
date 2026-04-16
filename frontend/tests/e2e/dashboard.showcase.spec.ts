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

  // P5 contract test: every fixture condition/action variant renders without
  // crash and produces recognisable text. If serde shape drifts, this breaks.
  test("policy page renders all condition + action variants from fixtures", async ({ page }) => {
    await page.goto("/policy");

    // 4 fixture rules should produce 4 rule cards (each has an #order prefix).
    await expect(page.getByText("#1")).toBeVisible();
    await expect(page.getByText("#4")).toBeVisible();

    // Condition text for each variant:
    // LegacyTokenTransferPresent → "legacy SPL Token Transfer detected"
    await expect(page.getByText("legacy SPL Token Transfer detected")).toBeVisible();
    // TokenAmountExceeds → "USDC transfer >"
    await expect(page.getByText(/USDC transfer/).first()).toBeVisible();
    // Always → "always"
    await expect(page.getByText("always").first()).toBeVisible();

    // Action text:
    // Reject → "reject" badge
    await expect(page.getByText("reject").first()).toBeVisible();
    // RequireApprovalChain → "chain" badge + stage roles
    await expect(page.getByText("chain").first()).toBeVisible();
    await expect(page.getByText("risk").first()).toBeVisible();
    await expect(page.getByText("treasury").first()).toBeVisible();
    await expect(page.getByText("cfo").first()).toBeVisible();
    // RequireHumanApproval → "human" badge
    await expect(page.getByText("human").first()).toBeVisible();
    // Approve → "approve" badge
    await expect(page.getByText("approve").first()).toBeVisible();
  });

  test("audit page renders fixture events", async ({ page }) => {
    await page.goto("/audit");
    await expect(page.getByRole("heading", { name: "Audit trail" })).toBeVisible();
    const body = page.locator("table");
    await expect(body).toContainText("human_approved");
    await expect(body).toContainText("approval_lease_expired");
  });
});
