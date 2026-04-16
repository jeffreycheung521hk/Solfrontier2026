import { test, expect, Page } from "@playwright/test";

// All live-mode tests share the same seeded snapshot produced by
// globalSetup. Each test navigates, waits for page text, then asserts.

const TOKEN = "showcase-demo-token";
const GATEWAY = "http://127.0.0.1:7070";

async function fetchPending(page: Page): Promise<Array<{ id: string; workflow: { expires_at?: string | null; stages: unknown[] }; verdict: { approval_chain?: unknown } }>> {
  const res = await page.request.get(`${GATEWAY}/pending-approvals`, {
    headers: { authorization: `Bearer ${TOKEN}` },
  });
  const body = await res.json();
  type Item = { request: { id: string; policy_verdict: { approval_chain?: unknown } }; workflow: { expires_at?: string | null; stages: unknown[] } };
  return (body.items as Item[]).map((i) => ({
    id: i.request.id,
    workflow: i.workflow,
    verdict: i.request.policy_verdict,
  }));
}

// Case 2: live mode mounts with the LIVE badge.
test("case2 live mode shows LIVE badge and seeded wallet pubkey", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Dashboard", exact: true })).toBeVisible();
  await expect(page.locator("aside").getByText(/^LIVE$/i)).toBeVisible();
  await expect(page.locator("body")).toContainText("devnet-hot-wallet");
});

// Case 3: dashboard numbers match the seed snapshot.
test("case3 dashboard reflects seeded state exactly", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Dashboard", exact: true })).toBeVisible();

  const pendingCard = page.getByTestId("stat-pending-approvals");
  await expect(pendingCard).toContainText("2");
  await expect(pendingCard).toContainText(/1 multi-stage/i);

  const walletCard = page.getByTestId("stat-wallets-monitored");
  await expect(walletCard).toContainText("1");

  // Rule names from seeded approvals.
  await expect(page.getByText("usdc-high-value-chain")).toBeVisible();
  await expect(page.getByText("large-transfer-requires-human")).toBeVisible();
});

// Case 4: proposal page in live mode must gracefully degrade — the full
// TransactionProposal is not exposed by the gateway yet, so the page shows
// an explanation instead of crashing.
test("case4 proposal page live-mode graceful degradation", async ({ page }) => {
  const items = await fetchPending(page);
  expect(items.length).toBeGreaterThan(0);
  const rid = items[0].id;

  await page.goto(`/proposal/${rid}`);
  // Degradation text appears in both the header pane and the instructions
  // pane. Asserting at least one is present is enough to prove the page
  // renders without the proposal blob.
  await expect(
    page.getByText(/not returned by the current gateway endpoints|not exposed by the live gateway/i).first(),
  ).toBeVisible();
});

// Case 5: approval chain view shows the risk → treasury workflow stages.
test("case5 approval chain view shows chain stages", async ({ page }) => {
  const items = await fetchPending(page);
  const chain = items.find((i) => !!i.verdict.approval_chain);
  expect(chain, "seed must produce at least one chain workflow").toBeDefined();

  await page.goto(`/approval/${chain!.id}`);
  // Page heading is the approval description; "Approval chain" is a small
  // kicker above it. Either one rendering proves the route loaded.
  await expect(page.getByText(/Approval chain/i).first()).toBeVisible();
  const body = page.locator("main");
  await expect(body).toContainText(/risk/i);
  await expect(body).toContainText(/treasury/i);
});

// Case 6: lease countdown decrements at ~1 s per tick.
//
// Re-seed first so we get a freshly-leased workflow regardless of how long
// ago globalSetup ran. The 5-minute chain lease is still valid by the time
// the page hydrates.
test("case6 lease countdown ticks down", async ({ page }) => {
  const reseed = await page.request.post(`${GATEWAY}/debug/seed-demo`, {
    headers: { authorization: `Bearer ${TOKEN}` },
  });
  expect(reseed.ok(), "re-seed must succeed to establish a fresh lease").toBe(true);

  const items = await fetchPending(page);
  // Pick the workflow whose expires_at is furthest in the future — that is
  // guaranteed to be the freshly-seeded chain (5-minute lease).
  const nowMs = Date.now();
  const leased = items
    .filter((i) => !!i.workflow.expires_at)
    .map((i) => ({ i, remaining: new Date(i.workflow.expires_at as string).getTime() - nowMs }))
    .filter((r) => r.remaining > 30_000)
    .sort((a, b) => b.remaining - a.remaining)[0]?.i;
  expect(leased, "re-seed must produce a workflow with at least 30s left").toBeDefined();

  await page.goto(`/approval/${leased!.id}`);

  const countdown = page.locator("span.font-mono.text-lg").first();
  await expect(countdown).toBeVisible();

  const parse = (s: string): number => {
    const m = s.match(/^(\d+):(\d{2})$/);
    if (!m) return Number.NaN;
    return Number(m[1]) * 60 + Number(m[2]);
  };

  // Wait for first non-placeholder reading. Production build hydration is
  // usually <1 s, but first-load in CI can take longer.
  await expect
    .poll(async () => parse((await countdown.textContent()) ?? ""), { timeout: 15_000 })
    .toBeGreaterThan(0);

  const before = parse((await countdown.textContent()) ?? "");
  await page.waitForTimeout(2_100);
  const after = parse((await countdown.textContent()) ?? "");
  expect(Number.isFinite(before) && Number.isFinite(after)).toBe(true);
  expect(after).toBeLessThan(before);
});

// Case 7: audit page surfaces all three seeded event types.
test("case7 audit page surfaces seeded event types", async ({ page }) => {
  await page.goto("/audit");
  await expect(page.getByRole("heading", { name: "Audit trail" })).toBeVisible();
  const body = page.locator("table");
  await expect(body).toContainText("human_approved");
  await expect(body).toContainText("approval_lease_expired");
  await expect(body).toContainText("policy_rejected");
});

// Case 8: policy page shows rules in config order and surfaces wallet info.
test("case8 policy page shows config-order rules and wallet info", async ({ page }) => {
  await page.goto("/policy");
  await expect(page.getByRole("heading", { name: "Policy", exact: true })).toBeVisible();

  const pageText = await page.locator("main").innerText();
  const idx = (needle: string) => pageText.indexOf(needle);
  const iLegacy   = idx("block-legacy-token-transfer");
  const iHighUsdc = idx("usdc-high-value-chain");
  const iMedUsdc  = idx("usdc-medium-value-requires-human");
  expect(iLegacy).toBeGreaterThanOrEqual(0);
  expect(iHighUsdc).toBeGreaterThan(iLegacy);
  expect(iMedUsdc).toBeGreaterThan(iHighUsdc);

  // Per-wallet tab shows the seeded wallet.
  await page.getByRole("tab", { name: /Per-wallet overrides/i }).click();
  await expect(page.locator("main").getByText("devnet-hot-wallet")).toBeVisible();
});
