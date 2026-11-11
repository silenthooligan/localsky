import { test, expect } from "@playwright/test";

test.use({ serviceWorkers: "block" });

for (const width of [1280, 390]) {
  test(`multiple soil probes persist aggregation, exclusion and timeout at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 900 });
    const ids = [1, 2, 3, 4].map(n => `source:fixture:soilmoisture${n}`);
    let stored: any = {
      schema_version: 3,
      deployment: { location: { lat: 51.5, lon: -0.1 }, units: "imperial" },
      sources: [], controllers: [{
        id: "fixture-controller", kind: "opensprinkler_direct", default: true, enabled: true,
        config: { host: "192.0.2.1", port: 80, password_md5: "fixture" },
      }],
      zones: { bed: {
        display_name: "Planter boxes", area_sqft: 100, species: "tall_fescue",
        soil_texture: "loam", sprinkler_type: "drip", controller_id: "fixture-controller",
        controller_station: "1", soil_sensor_id: ids[0],
        target_min_pct_soil: 30, saturation_pct_soil: 70,
      } },
    };
    const writes: any[] = [];
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("**/api/**", async route => {
      const request = route.request();
      const path = new URL(request.url()).pathname.replace("/api/v1/", "/api/");
      if (path === "/api/config") {
        if (request.method() === "PUT") {
          stored = request.postDataJSON();
          writes.push(stored);
          return route.fulfill({ json: { saved: 1, restart_required: false, restart_reasons: [] } });
        }
        return route.fulfill({ json: stored });
      }
      if (path === "/api/sensors/soil") return route.fulfill({ json: ids.map((id, n) => ({
        id, label: `Probe ${n+1}`, current_pct: [74, 40, 36, 29][n], source: "fixture",
      })) });
      if (!["GET", "HEAD"].includes(request.method())) return route.fulfill({ status: 409, json: { error: "Unexpected fixture write" } });
      return route.continue();
    });
    const openEditor = async () => {
      await page.goto("/settings/zones?edit=bed", { waitUntil: "domcontentloaded" });
      await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
      await expect(page.getByRole("textbox", { name: "Name", exact: true })).toHaveValue("Planter boxes");
      await page.locator(".zone-form-advanced__summary").click();
    };
    await openEditor();
    await expect(page.locator(".soil-probe-editor__row")).toHaveCount(1);
    for (const id of ids.slice(1)) await page.getByRole("combobox", { name: "Add soil probe", exact: true }).selectOption(id);
    await expect(page.locator(".soil-probe-editor__row")).toHaveCount(4);
    await page.getByRole("combobox", { name: "Combine soil readings" }).selectOption("minimum");
    await page.getByRole("checkbox", { name: "Exclude Probe 1 from decisions", exact: true }).check();
    await page.getByRole("spinbutton", { name: "Stale timeout for Probe 2", exact: true }).fill("900");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath(`soil-probes-${width}.png`), fullPage: true });
    await page.getByRole("button", { name: "Save zone changes", exact: true }).click();
    await expect.poll(() => writes.length).toBe(1);
    const zone = stored.zones.bed;
    expect(zone.soil_sensor_id).toBeNull();
    expect(zone.soil_aggregation).toBe("minimum");
    expect(zone.soil_probes.map((p: any) => p.sensor_id)).toEqual(ids);
    expect(zone.soil_probes[0].excluded_from_decisions).toBe(true);
    expect(zone.soil_probes[1].stale_after_s).toBe(900);
    await openEditor();
    await expect(page.locator(".soil-probe-editor__row")).toHaveCount(4);
    await expect(page.getByRole("checkbox", { name: "Exclude Probe 1 from decisions", exact: true })).toBeChecked();
    await expect(page.getByRole("combobox", { name: "Combine soil readings" })).toHaveValue("minimum");
    await expect(page.getByRole("spinbutton", { name: "Stale timeout for Probe 2", exact: true })).toHaveValue("900");
    expect(errors).toEqual([]);
  });
}
