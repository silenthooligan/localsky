import { test, expect } from "@playwright/test";

test.use({ serviceWorkers: "block" });

test("a denied preferred radar falls back with truthful attribution", async ({ page }) => {
  await page.route("https://api.librewxr.net/**", route => route.fulfill({ status: 403, body: "Denied" }));
  await page.route("https://api.rainviewer.com/public/weather-maps.json", route => route.fulfill({ json: {
    host: "https://tilecache.rainviewer.com", radar: { past: [{ time: Math.floor(Date.now() / 1000), path: "/v2/radar/test-frame" }] },
  } }));
  await page.route("https://tilecache.rainviewer.com/**", route => route.fulfill({
    contentType: "image/png",
    body: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=", "base64"),
  }));
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  await expect(page.locator("#radar-time")).toHaveAttribute("data-provider", "rainviewer");
  await expect(page.locator("#radar-time")).toContainText("via RainViewer.com");
  // The geographic base map is independent of radar. Its former CARTO URL
  // returns an access-error image with HTTP 200 on an install without a key.
  const baseTile = page.locator("img.leaflet-tile[src^='https://tile.openstreetmap.org/']").first();
  await expect(baseTile).toBeAttached();
  await expect(baseTile).toHaveAttribute("referrerpolicy", "strict-origin-when-cross-origin");
  await expect(page.locator(".leaflet-control-attribution a[href='https://www.openstreetmap.org/copyright']")).toBeVisible();
  await expect(page.locator("img.leaflet-tile[src*='cartocdn']")).toHaveCount(0);
  await expect(page.locator("img.leaflet-tile[src*='mesonet.agron.iastate.edu']").first()).toBeAttached();
});

test("unavailable animated radar is reported while regional imagery remains", async ({ page }) => {
  await page.route("https://api.librewxr.net/**", route => route.fulfill({ status: 403, body: "Denied" }));
  await page.route("https://api.rainviewer.com/**", route => route.fulfill({ json: { radar: { past: [] } } }));
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  await expect(page.locator("#radar-time")).toHaveText("Radar unavailable");
  await expect(page.locator("img.leaflet-tile[src*='mesonet.agron.iastate.edu']").first()).toBeAttached();
});
