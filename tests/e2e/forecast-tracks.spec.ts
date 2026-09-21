import { test, expect, Page } from "@playwright/test";
import visualDemo from "./fixtures/visual-demo.json";

test.use({ serviceWorkers: "block" });

async function fixture(page: Page, pendingSetup = false) {
  let stored: any = {
    schema_version: 2,
    deployment: { location: { lat: 39, lon: -104 }, units: "imperial" },
    sources: [], controllers: [], zones: {}, forecast_tracks: [],
  };
  const writes: any[] = [];
  const unexpected: string[] = [];
  const irrigation = {
    ...(visualDemo.responses as Record<string, any>)["/api/irrigation/snapshot"],
    zones: [], restart_required: pendingSetup,
    restart_reasons: pendingSetup ? [
      "Your controllers are not connected yet (controller connections changed).",
      "Your zones are not on the watering schedule yet (zone configuration changed).",
      "Your weather sources are not connected yet (weather source connections changed).",
    ] : [],
  };
  let reject = false;
  await page.route("**/api/**", async route => {
    const req = route.request();
    const path = new URL(req.url()).pathname.replace("/api/v1/", "/api/");
    if (path === "/api/config") {
      if (req.method() === "GET") return route.fulfill({ json: stored });
      if (req.method() === "PUT") {
        writes.push(req.postDataJSON());
        if (reject) return route.fulfill({ status: 503, body: "Save unavailable" });
        stored = req.postDataJSON();
        return route.fulfill({ json: { saved: 1, restart_required: false, restart_reasons: [] } });
      }
    }
    if (path === "/api/forecast/tracks") return route.fulfill({ json: [] });
    // Keep runtime notices consistent with this config, independent of the
    // fresh-install test server. The smallest viewport exercises a real hold.
    if (path === "/api/irrigation/snapshot") return route.fulfill({ json: irrigation });
    if (path === "/api/irrigation/stream") return route.fulfill({
      contentType: "text/event-stream",
      body: `event: snapshot\ndata: ${JSON.stringify(irrigation)}\n\n`,
    });
    if (!["GET", "HEAD"].includes(req.method())) {
      unexpected.push(path);
      return route.fulfill({ status: 409, body: "Unexpected fixture write" });
    }
    return route.continue();
  });
  return { writes, unexpected, reject: () => { reject = true; } };
}

for (const width of [1280, 390, 320]) {
  test(`forecast model changes persist without replacing other settings at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    const state = await fixture(page, width === 320);
    await page.goto("/settings/devices");
    await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
    const card = page.getByRole("region", { name: "Extra forecast models" });
    if (width === 320) {
      const notice = page.locator(".health-banner--sticky");
      await expect(notice).toBeVisible();
      expect((await notice.boundingBox())!.height).toBeLessThanOrEqual(315);
    }
    await card.getByRole("textbox", { name: "Short name" }).fill("nbm");
    await card.getByRole("combobox", { name: "Weather model" }).selectOption("ncep_nbm_conus");
    await expect(card.getByText(/^Coverage:/)).toBeVisible();
    await card.getByRole("button", { name: "Add model" }).click();
    await expect(card.locator("li")).toHaveCount(1);
    expect(state.writes[0].forecast_tracks).toEqual([{ id: "nbm", model: "ncep_nbm_conus" }]);
    expect(state.writes[0].deployment.location).toEqual({ lat: 39, lon: -104 });
    await expect(page.getByText("Forecast models saved.", { exact: true })).toBeVisible();
    await page.reload();
    await expect(card.locator("li")).toHaveCount(1);
    await expect(card.getByText("Waiting for its first forecast", { exact: true })).toBeVisible();
    const bounds = await card.boundingBox();
    expect(bounds).not.toBeNull();
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(width + 1);
    await card.getByRole("button", { name: "Remove", exact: true }).click();
    await expect(card.locator("li")).toHaveCount(0);
    expect(state.writes.at(-1).forecast_tracks).toEqual([]);
    expect(state.unexpected).toEqual([]);
  });
}

test("a rejected forecast model save keeps the form and reports the failure", async ({ page }) => {
  const state = await fixture(page);
  state.reject();
  await page.goto("/settings/devices");
  const card = page.getByRole("region", { name: "Extra forecast models" });
  await card.getByRole("textbox", { name: "Short name" }).fill("nbm");
  await card.getByRole("button", { name: "Add model" }).click();
  await expect(card.getByRole("alert")).toBeVisible();
  await expect(card.locator("li")).toHaveCount(0);
  await expect(card.getByRole("textbox", { name: "Short name" })).toHaveValue("nbm");
  expect(state.unexpected).toEqual([]);
});
