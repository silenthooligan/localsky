import { test, expect } from "@playwright/test";

test.use({ serviceWorkers: "block" });

test("demo history has recent watering and daily skip reasons", async ({ page, request }) => {
  await expect.poll(async () => {
    const response = await request.get("/api/v1/irrigation/history?days=30");
    const body = await response.json();
    return body.runs.filter((run: { duration_s: number }) => run.duration_s > 0).length;
  }).toBeGreaterThan(20);
  const history = await (await request.get("/api/v1/irrigation/history?days=30")).json();
  expect(history.daily.length).toBeGreaterThan(20);
  expect(history.daily.some((day: { zones: { planned_seconds: number; reason: string }[] }) => day.zones.every(zone => zone.planned_seconds === 0 && zone.reason.length > 0))).toBe(true);
  await page.goto("/history", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  const totals = page.locator(".hist-kpis .stat-tile__value");
  // Check the rendered default insights, not only the backing API. Watering
  // time, sessions and skipped mornings must all survive the UI rollups.
  await expect(totals).toHaveCount(4);
  for (const index of [0, 1, 2]) {
    await expect.poll(async () => Number((await totals.nth(index).innerText()).replaceAll(",", ""))).toBeGreaterThan(0);
  }
  await page.screenshot({ path: "test-results/demo-history.png" });
  // A full-year run log can exceed the browser's screenshot texture limit.
  // Capture the actual viewport after scrolling to the insights instead.
  await page.locator(".hist-insights-heading").scrollIntoViewIfNeeded();
  await page.screenshot({ path: "test-results/demo-history-insights.png" });
});

test("demo setup explains read-only mode and keeps the write guard", async ({ page, request }) => {
  await page.goto("/setup", { waitUntil: "domcontentloaded" });
  await expect(page.locator(".setup-demo-intro")).toBeVisible();
  await expect(page.locator(".setup-demo-intro")).toContainText("This demo is read-only");
  await expect(page.locator(".setup-live-intro")).toBeHidden();
  const startFresh = page.getByRole("button", { name: "Start fresh", exact: true });
  if (await startFresh.isVisible()) await startFresh.click();
  await expect(page.locator(".setup-footer [role=alert]")).toHaveText("This demo is read-only. Changes and device probes are disabled.");
  await expect(page.locator(".setup-shell")).not.toContainText("LS_API_REJECTED");
  const response = await request.put("/api/v1/wizard/draft", { data: {} });
  expect(response.status()).toBe(403);
  expect((await response.json()).error).toBe("demo_read_only");
  await page.locator(".setup-shell").screenshot({ path: "test-results/demo-setup.png" });
});
