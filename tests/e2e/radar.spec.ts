import { test, expect } from "@playwright/test";

test.use({ serviceWorkers: "block" });

test("the base map follows every theme without recoloring weather overlays", async ({ page }) => {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  const base = page.locator(".radar-basemap");
  await expect(base.locator("img.leaflet-tile").first()).toBeAttached();
  const filter = () => base.evaluate(element => getComputedStyle(element).filter);
  for (const theme of ["dark", "light", "hc", "auto"]) {
    await page.evaluate(value => document.documentElement.setAttribute("data-theme", value), theme);
    await page.emulateMedia({ colorScheme: "dark" });
    if (theme === "light") await expect.poll(filter).toBe("none");
    else await expect.poll(filter).not.toBe("none");
    const otherPanes = await page.locator("#radar-map .leaflet-pane:not(.radar-basemap)").evaluateAll(elements => elements.map(element => getComputedStyle(element).filter));
    expect(otherPanes.every(value => value === "none")).toBe(true);
    if (theme === "auto") {
      await page.emulateMedia({ colorScheme: "light" });
      await expect.poll(filter).toBe("none");
      await page.emulateMedia({ colorScheme: "dark" });
      await expect.poll(filter).not.toBe("none");
    }
  }
  await page.evaluate(() => document.documentElement.setAttribute("data-theme", "dark"));
  await page.locator(".radar-map-shell").screenshot({ path: "test-results/radar-dark.png" });
  await page.evaluate(() => document.documentElement.setAttribute("data-theme", "light"));
  await page.locator(".radar-map-shell").screenshot({ path: "test-results/radar-light.png" });
});

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
