import { test, expect } from "@playwright/test";

test.use({ serviceWorkers: "block" });

test("demo simulator calculates without changing configuration or run history", async ({ page, request }) => {
  const config = await (await request.get('/api/v1/config')).json();
  const runs = (await (await request.get('/api/v1/irrigation/history?days=30')).json()).runs;
  await page.goto('/simulator');
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  await expect(page.locator('.sim-verdict')).toBeVisible();
  const response = await request.post('/api/v1/irrigation/simulate', { data: { wind_now_mph: 100 } });
  expect(response.status()).toBe(200);
  expect((await response.json()).hypothetical.verdict).toBe('skip');
  expect(await (await request.get('/api/v1/config')).json()).toEqual(config);
  expect((await (await request.get('/api/v1/irrigation/history?days=30')).json()).runs).toEqual(runs);
  // Exercise the guard with an invalid action, so this can never start a valve.
  const blocked = await request.post('/api/v1/irrigation/action', { data: {} });
  expect(blocked.status()).toBe(403);
  expect((await blocked.json()).error).toBe('demo_read_only');
});

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
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  await expect(page.locator(".setup-footer [role=alert]")).toHaveText("This demo is read-only. Changes and device probes are disabled.");
  await expect(page.locator(".setup-shell")).not.toContainText("LS_API_REJECTED");
  const response = await request.put("/api/v1/wizard/draft", { data: {} });
  expect(response.status()).toBe(403);
  expect((await response.json()).error).toBe("demo_read_only");
  await page.locator(".setup-shell").screenshot({ path: "test-results/demo-setup.png" });
  await page.getByRole("combobox", { name: "Jump to step", exact: true }).selectOption("controllers");
  await expect(page).toHaveURL(/\/setup\/controllers$/);
  await expect(page.getByRole("button", { name: "+ Add a controller", exact: true })).toBeVisible();
  // A direct visit starts a new shell too. Both routes must stay explorable
  // while every real write continues to receive the server's 403.
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  await expect(page.getByRole("button", { name: "+ Add a controller", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Edit current setup", exact: true })).toHaveCount(0);
});
