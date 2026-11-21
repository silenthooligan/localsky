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

for (const width of [1280, 390]) {
  test(`a removed Home Assistant probe leaves the lists and can be shown again at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 900 });
    const id = "ha:sensor.old_planter_soil_moisture";
    let stored: any = {
      schema_version: 3,
      deployment: { location: { lat: 51.5, lon: -0.1 }, units: "imperial" },
      sources: [], controllers: [], zones: {}, ui: { hidden_soil_probes: [] },
    };
    const writes: any[] = [];
    const removes: any[] = [];
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    const inventory = () => {
      const listed = !stored.ui.hidden_soil_probes.includes(id);
      return {
        gateways: listed ? [{ source_id: "home_assistant", label: "Home Assistant", kind: "home_assistant", online: true, soil_count: 1 }] : [],
        soil: listed ? [{
          id, channel_label: "Old planter soil moisture (HA)", source_id: "home_assistant",
          source_label: "Home Assistant", source_kind: "home_assistant", moisture_pct: 31, age_s: 60,
        }] : [],
        flow: [],
      };
    };
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
      if (path === "/api/sensors/inventory") return route.fulfill({ json: inventory() });
      if (path === "/api/sensors/soil/remove" && request.method() === "POST") {
        removes.push(request.postDataJSON());
        stored.ui.hidden_soil_probes.push(id);
        return route.fulfill({ json: { zones_unbound: 0, gateway: "skipped", detail: "kept out of the sensor lists" } });
      }
      if (!["GET", "HEAD"].includes(request.method())) return route.fulfill({ status: 409, json: { error: "Unexpected fixture write" } });
      return route.continue();
    });
    await page.goto("/settings/sensors", { waitUntil: "domcontentloaded" });
    await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
    const probe = page.getByText("Old planter soil moisture (HA)", { exact: true });
    await expect(probe).toBeVisible();
    await expect(page.locator(".hidden-probes__row")).toHaveCount(0);

    await page.getByRole("button", { name: "Remove probe", exact: true }).click();
    await expect(page.getByText("LocalSky clears its zone binding and stops listing it.", { exact: false })).toBeVisible();
    await page.getByRole("button", { name: "Remove", exact: true }).click();
    await expect.poll(() => removes.length).toBe(1);
    expect(removes[0]).toEqual({ probe_id: id, also_gateway: true });

    // It is gone from the probe cards and offered back under Removed probes.
    await expect(probe).toHaveCount(0);
    const row = page.locator(".hidden-probes__row");
    await expect(row).toHaveCount(1);
    await expect(row).toContainText("sensor.old_planter_soil_moisture (Home Assistant)");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath(`removed-probes-${width}.png`), fullPage: true });

    await page.getByRole("button", { name: "Show sensor.old_planter_soil_moisture (Home Assistant) again", exact: true }).click();
    await expect.poll(() => writes.length).toBe(1);
    expect(stored.ui.hidden_soil_probes).toEqual([]);
    await expect(probe).toBeVisible();
    await expect(row).toHaveCount(0);
    expect(errors).toEqual([]);
  });
}
