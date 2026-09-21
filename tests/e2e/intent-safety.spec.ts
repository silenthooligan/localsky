import visualDemo from "./fixtures/visual-demo.json";
import { test, expect, Page } from "@playwright/test";

// Run against an isolated configured demo/fresh-install instance. All writes,
// including vendor scans, are intercepted; no action can reach a valve.
test.use({ serviceWorkers: "block" });

const config = {
  schema_version: 2,
  deployment: { location: { lat: 51.5, lon: -0.1 }, units: "imperial" },
  sources: [],
  controllers: [
    { id: "fixture_a", kind: "rachio", enabled: true, default: true,
      config: { api_token: "fixture-only", device_id: "fixture", zone_uuid_map: {} } },
    { id: "fixture_b", kind: "dry_run", enabled: true, default: false, config: {} },
  ],
  zones: { fixture_zone: {
    display_name: "Fixture lawn", species: "tall_fescue", soil_texture: "loam",
    sprinkler_type: "rotor", area_sqft: 1000, controller_id: "fixture_a",
    controller_station: "", scheduling_model: "weekly",
  } },
};

async function fixture(page: Page, restartReasons?: string[]) {
  const writes: any[] = [];
  let irrigation: any = null;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname.replace("/api/v1/", "/api/");
    if (path === "/api/config" && request.method() === "GET") {
      // The list must arrive after mount, as it does on a real connection.
      await new Promise((resolve) => setTimeout(resolve, 300));
      return route.fulfill({ json: config });
    }
    if (path === "/api/sensors/soil") return route.fulfill({ json: [] });
    if (path === "/api/irrigation/soil-invite") return route.fulfill({ json: { eligible: false } });
    if (path === "/api/irrigation/stream") {
      if (!irrigation) {
        const response = await page.request.get("/api/v1/irrigation/snapshot");
        irrigation = await response.json();
        irrigation.global_override = "auto";
        irrigation.water_budgets = [];
        irrigation.ha_adoption = [];
        irrigation.restart_required = restartReasons !== undefined;
        irrigation.restart_reasons = restartReasons ?? [];
        irrigation.zones = [{
          name: "Fixture lawn", slug: "fixture_zone", hex: "", running: false,
          running_known: true, override_mode: "auto", planned_run_seconds: 600,
          last_run_epoch: 0, bucket_mm: null,
        }];
      }
      return route.fulfill({ contentType: "text/event-stream",
        body: `event: snapshot\ndata: ${JSON.stringify(irrigation)}\n\n` });
    }
    if (!["GET", "HEAD"].includes(request.method())) {
      writes.push(request.postDataJSON());
      if (path === "/api/wizard/scan_zones") {
        return route.fulfill({ json: { zones: [
          { station_id: "fixture-station", name: "Fixture sprinkler" },
        ] } });
      }
      if (path === "/api/irrigation/action") {
        const action = request.postDataJSON();
        if (irrigation && action.kind === "set_global_override") irrigation.global_override = action.mode;
        if (irrigation && action.kind === "set_zone_override") irrigation.zones[0].override_mode = action.mode;
        return route.fulfill({ json: { ok: true } });
      }
      if (path === "/api/config" && request.method() === "PUT") {
        return route.fulfill({ json: { saved: 1, restart_required: false, restart_reasons: [] } });
      }
      return route.fulfill({ status: 409, json: { error: "Unexpected fixture write" } });
    }
    return route.continue();
  });
  return writes;
}

test("address parity saves when selected without an unrelated restriction edit", async ({ page }) => {
  const writes = await fixture(page);
  await open(page, "/settings/restrictions");
  const parity = page.getByRole("radiogroup", { name: "Address parity" });
  await expect(parity).toBeVisible();
  await expect(page.getByRole("radio", { name: "N/A", exact: true })).toHaveAttribute("aria-checked", "true");
  // Let the delayed config read complete before changing the form.
  await page.waitForTimeout(400);
  await parity.getByRole("radio", { name: "Odd", exact: true }).click();
  await expect.poll(() => writes.length).toBe(1);
  expect(writes[0].deployment.address_parity).toBe("odd");
  await expect(page.getByText(/Saved\./).first()).toBeVisible();
});

test("a pending runtime restart stays visible across navigation and another save", async ({ page }) => {
  const writes = await fixture(page, ["Fixture controller bindings changed."]);
  await open(page, "/irrigation");
  const notice = page.locator(".health-banner").filter({ hasText: "Restart required to apply." });
  await expect(notice).toHaveCount(1);
  await expect(notice).toContainText("New watering is on hold");
  await expect(notice).toContainText("Fixture controller bindings changed.");
  await expect(notice.getByRole("button", { name: /Dismiss/ })).toHaveCount(0);
  await page.locator(".sidebar").getByRole("link", { name: "Settings", exact: true }).click();
  await page.getByRole("button", { name: /Restrictions.*Day-of-week/ }).click();
  await expect(notice).toBeVisible();
  await page.getByRole("radiogroup", { name: "Address parity" }).getByRole("radio", { name: "Odd", exact: true }).click();
  await expect.poll(() => writes.length).toBe(1);
  // This unrelated fixture PUT says restart_required=false; the authoritative
  // runtime snapshot still holds watering and the global notice stays put.
  await expect(notice).toBeVisible();
  await page.reload({ waitUntil: "domcontentloaded" });
  await expect(notice).toBeVisible();
});

test("HA entity prefix names the configured zone and saves only an explicit change", async ({ page }) => {
  const writes = await fixture(page);
  await page.route("**/api/v1/health", async (route) => {
    if (route.request().method() !== "GET") return route.abort();
    const response = await route.fetch();
    const health = await response.json();
    health.ha = { env_configured: true, reachable: false, snapshot_source: "home_assistant" };
    return route.fulfill({ json: health });
  });
  await open(page, "/settings/home-assistant");
  await page.locator("summary.ha-advanced__summary").click();
  const prefix = page.getByRole("textbox", { name: "HA controller entity prefix", exact: true });
  await expect(prefix).toHaveValue("opensprinkler");
  const save = page.getByRole("button", { name: "Save HA entity prefix", exact: true });
  await expect(save).toBeDisabled();
  expect(writes).toHaveLength(0);
  await prefix.fill("switch.bad value");
  await expect(save).toBeDisabled();
  await expect(prefix).toHaveAttribute("aria-invalid", "true");
  await prefix.fill("yard_controller");
  await expect(page.getByText("switch.yard_controller_enabled", { exact: true })).toBeVisible();
  await expect(page.getByText("sensor.yard_controller_water_level", { exact: true })).toBeVisible();
  await expect(page.getByText("binary_sensor.yard_controller_fixture_zone_station_running", { exact: true })).toBeVisible();
  await save.click();
  await expect.poll(() => writes.length).toBe(1);
  expect(writes[0].deployment.ha_sprinkler_prefix).toBe("yard_controller");
  expect(writes[0].controllers).toEqual(config.controllers);
  await expect(save).toBeDisabled();
});

async function open(page: Page, path: string) {
  await page.goto(path, { waitUntil: "domcontentloaded" });
  await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
}

for (const width of [1280, 390]) {
  test(`Zones edits stay native at ${width}px and Cancel preserves an unsaved draft`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    const writes = await fixture(page);
    await open(page, "/zones");
    if (width === 390) {
      await page.getByRole("button", { name: "Open Fixture lawn details", exact: true }).click();
      await expect(page).toHaveURL(/\/zones\/fixture_zone$/);
    }
    const nativePath = new URL(page.url()).pathname;
    await page.getByRole("link", { name: "Edit zone", exact: true }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(nativePath);
    await expect.poll(() => new URL(page.url()).searchParams.get("edit")).toBe("fixture_zone");
    const drawer = page.locator(".zone-editor-host .sheet--drawer");
    await expect(drawer).toHaveClass(/sheet--open/);
    await expect(drawer.getByPlaceholder("Back Yard")).toHaveValue("Fixture lawn");
    await page.waitForTimeout(400);
    const bounds = await drawer.locator(":scope > .sheet__panel").boundingBox();
    expect(bounds!.width).toBeGreaterThan(width === 390 ? 360 : 450);
    expect(bounds!.height).toBeGreaterThan(650);
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
    expect(bounds!.y).toBeGreaterThanOrEqual(0);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(width + 1);
    expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(901);
    await expect(page.locator(".settings-page__header")).toHaveCount(0);
    await drawer.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(drawer).not.toHaveClass(/sheet--open/);
    await expect.poll(() => new URL(page.url()).pathname + new URL(page.url()).search).toBe(nativePath);
    await page.getByRole("link", { name: "Edit zone", exact: true }).click();
    await drawer.getByPlaceholder("Back Yard").fill("Unsaved native draft");
    await drawer.getByRole("button", { name: "Cancel", exact: true }).click();
    const confirm = page.getByLabel("Discard this zone?", { exact: true });
    await expect(confirm).toBeVisible();
    await confirm.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(drawer.getByPlaceholder("Back Yard")).toHaveValue("Unsaved native draft");
    await drawer.getByRole("button", { name: "Cancel", exact: true }).click();
    await confirm.getByRole("button", { name: "Discard", exact: true }).click();
    await expect.poll(() => new URL(page.url()).pathname + new URL(page.url()).search).toBe(nativePath);
    expect(writes).toEqual([]);
  });
}

test("a native zone save keeps a failed draft, then closes with the server confirmation", async ({ page }) => {
  const writes = await fixture(page);
  let fail = true;
  await page.route("**/api/config", async route => {
    if (route.request().method() !== "PUT") return route.fallback();
    writes.push(route.request().postDataJSON());
    if (fail) return route.fulfill({ status: 503, json: { error: "Fixture save unavailable" } });
    return route.fulfill({ json: { status: "ok", restart_required: true, restart_reasons: ["Fixture binding changed"] } });
  });
  await open(page, "/zones?edit=fixture_zone");
  const drawer = page.locator(".zone-editor-host .sheet--drawer");
  await expect(drawer.getByPlaceholder("Back Yard")).toHaveValue("Fixture lawn");
  await drawer.getByPlaceholder("Back Yard").fill("Saved native zone");
  await drawer.getByRole("button", { name: "Save zone changes", exact: true }).click();
  await expect(drawer.getByRole("alert")).toBeVisible();
  await expect(drawer.getByPlaceholder("Back Yard")).toHaveValue("Saved native zone");
  fail = false;
  await drawer.getByRole("button", { name: "Save zone changes", exact: true }).click();
  await expect(page).toHaveURL(/\/zones$/);
  await expect(page.getByText("Saved. Restart LocalSky to use it.", { exact: true }).first()).toBeVisible();
  expect(writes).toHaveLength(2);
  expect(writes[1].zones.fixture_zone.display_name).toBe("Saved native zone");
});

test("Irrigation explains the recorded morning and uses water need instead of weather exemptions", async ({ page }) => {
  const writes = await fixture(page);
  const now = new Date("2026-09-12T18:00:00Z");
  await page.clock.setFixedTime(now);
  const snapshot = { ...(visualDemo.responses as Record<string, any>)["/api/irrigation/snapshot"],
    zones: (visualDemo.responses as Record<string, any>)["/api/irrigation/snapshot"].zones.map((z: any) => ({ ...z, running_known: true })),
    timezone: "America/New_York", next_run_epoch: 0, next_run_state: "no_water_planned",
    next_run_day_offset: null, last_refresh_epoch: now.getTime() / 1000,
    seven_day_verdicts: [{ day_offset: 1, time_epoch: Date.parse("2026-09-13T04:00:00Z") / 1000,
      weather_code: 61, verdict: "run", mixed_hold: true, reason: "Weather exemption", reason_code: "soil_model" }],
    water_plan: [{ date_local: "2026-09-13", day_offset: 1, time_epoch: Date.parse("2026-09-13T04:00:00Z") / 1000,
      start_epoch: null, finish_epoch: null, forecast_rain_mm: 6.35, expected_rain_mm: 4, rain_probability_pct: 70,
      evidence_complete: true, zones: [{ zone: "front", name: "Front", planned_seconds: 0,
        reason: "Recent rain keeps the roots below the watering trigger", reason_code: "water_balance",
        water_need: "No water needed", depletion_mm: 0, depletion_range_mm: [0, 1], trigger_mm: 10, capacity_mm: 20,
        demand_mm: 3, demand_source: "forecast_et0", model: "soil", session_capped: false }] }],
  };
  await page.route("**/api/irrigation/stream", route => route.fulfill({ contentType: "text/event-stream", body: `event: snapshot\ndata: ${JSON.stringify(snapshot)}\n\n` }));
  await page.route("**/api/irrigation/history?days=2", route => route.fulfill({ json: {
    from_epoch: 0, to_epoch: now.getTime() / 1000, runs: [], daily: [{ date_local: "2026-09-12",
      epoch: Date.parse("2026-09-12T10:00:00Z") / 1000, kind: "scheduled_legacy",
      zones: [{ zone: "front", name: "Front", planned_seconds: 0, reason_code: "soil_not_due",
        reason: "The soil model did not request watering at the scheduled morning decision", water_need: "" }] }],
  } }));
  await open(page, "/irrigation");
  const overview = page.locator(".irrigation-overview");
  await expect(overview.getByRole("heading", { level: 1 })).toHaveText("Skipped");
  await expect(overview.locator(".irrigation-overview__today")).toContainText("Soil has enough water");
  await expect(overview.locator(".irrigation-overview__tomorrow")).toContainText("Not watering");
  await expect(overview.locator(".irrigation-overview__tomorrow")).toContainText("Projected");
  await expect(page.locator(".water-plan")).toHaveCount(0);
  await expect(overview).not.toContainText("06:53");
  expect((await overview.innerText()).split(/\s+/).length).toBeLessThan(65);
  await overview.getByRole("link", { name: "Watering decisions" }).click();
  await expect(page).toHaveURL(/\/irrigation\/decisions$/);
  const plan = page.locator(".water-plan");
  await expect(plan.locator(".water-plan__heading h2")).toHaveText("No watering planned for tomorrow");
  await expect(plan.locator(".water-plan__today")).toContainText("No automatic watering requested");
  await expect(plan.locator(".water-plan__day").first()).toContainText("Recent rain keeps the roots below the watering trigger");
  await expect(plan).not.toContainText("Some zones may water");
  await expect(plan).not.toContainText("06:53");
  await expect(plan.locator(".water-plan__timing")).toHaveCount(0);
  const day = plan.locator(".water-plan__day").first();
  const dayToggle = day.locator(":scope > summary");
  await expect(dayToggle).toContainText("Hide zones");
  await dayToggle.focus();
  await page.keyboard.press("Enter");
  await expect(day).not.toHaveAttribute("open", "");
  await expect(dayToggle.locator(".disclosure-closed")).toBeVisible();
  await page.keyboard.press("Space");
  await expect(day).toHaveAttribute("open", "");
  const evidence = day.locator(".plan-zone__detail").first();
  await evidence.locator("summary").click();
  await expect(evidence.locator(".plan-zone__evidence")).toBeVisible();
  snapshot.water_plan[0].zones[0].reason += " Updated evidence.";
  snapshot.last_refresh_epoch += 10;
  // The fixture stream reconnects with a new snapshot, like a real SSE refresh.
  // Reading a zone must not collapse its evidence when those values update.
  await expect(evidence).toContainText("Updated evidence.");
  await expect(evidence).toHaveAttribute("open", "");
  await expect(day).toHaveAttribute("open", "");
  await plan.getByText("How timing and water need are calculated", { exact: true }).click();
  await expect(plan).toContainText("finish 15 minutes before sunrise");
  await expect(plan).toContainText("A zero-minute plan has no watering start");
  await page.getByRole("link", { name: "← Irrigation", exact: true }).click();
  await expect(page).toHaveURL(/\/irrigation$/);
  await expect(page.locator(".irrigation-overview")).toBeVisible();
  await expect(page.locator(".water-plan")).toHaveCount(0);
  expect(writes).toEqual([]);
});

test("All months restores older runs and the daily log includes waterless mornings", async ({ page }) => {
  const writes = await fixture(page);
  const now = new Date("2026-09-12T18:00:00Z");
  await page.clock.setFixedTime(now);
  const all = { from_epoch: 0, to_epoch: now.getTime() / 1000,
    runs: [{ zone: "old_garden", start_epoch: Date.parse("2026-05-10T11:00:00Z") / 1000,
      duration_s: 600, source: "smart_morning", status: "completed", skip_reason: null }],
    daily: [{ date_local: "2026-09-12", epoch: Date.parse("2026-09-12T10:52:00Z") / 1000,
      kind: "scheduled", zones: [{ zone: "garden", name: "Garden", planned_seconds: 0,
        reason_code: "water_balance", reason: "Recent measured rain filled the root zone", water_need: "No water needed" }] }],
  };
  const requests: string[] = [];
  await page.route("**/api/irrigation/history?**", route => {
    const days = new URL(route.request().url()).searchParams.get("days");
    requests.push(days ?? "");
    return route.fulfill({ json: { ...all, runs: days === "0" ? all.runs : [] } });
  });
  await open(page, "/history");
  await expect(page.locator(".hist-panel").first()).toContainText("old garden");
  await page.getByRole("group", { name: "Run log range" }).getByRole("button", { name: "7d", exact: true }).click();
  await expect(page.locator(".hist-panel").first()).not.toContainText("old garden");
  await page.getByLabel("Jump to a month").selectOption("");
  await expect(page.locator(".hist-panel").first()).toContainText("old garden");
  await page.getByRole("group", { name: "History view" }).getByRole("button", { name: "Daily log", exact: true }).click();
  const daily = page.locator(".daily-log");
  await daily.locator("summary").filter({ hasText: "2026-09-12" }).click();
  await expect(daily).toContainText("Recent measured rain filled the root zone");
  expect(requests.filter(value => value === "0").length).toBeGreaterThanOrEqual(2);
  expect(writes).toEqual([]);
});

test("History leads with the run log and a failed load cannot look like zero watering", async ({ page }) => {
  const writes = await fixture(page);
  await page.route("**/api/irrigation/history?**", route => route.fulfill({ status: 503, body: "Unavailable" }));
  await open(page, "/history");
  await expect(page.locator(".hist-panel__title").first()).toHaveText("Run log");
  await expect(page.getByText("Run records could not be loaded.", { exact: false })).toBeVisible();
  const review = page.locator("details.hist-forecast-review");
  await expect(review).not.toHaveAttribute("open", "");
  await expect(page.locator(".hist-kpis")).toHaveCount(0);
  expect(writes).toEqual([]);
});

async function sourceFixture(page: Page, source: any) {
  let stored: any = { ...config, sources: [source], controllers: [], zones: {} };
  const irrigation: any = {
    ...(visualDemo.responses as Record<string, any>)["/api/irrigation/snapshot"],
    zones: [], restart_required: false, restart_reasons: [],
  };
  const writes: any[] = [];
  const unexpected: string[] = [];
  await page.route("**/api/**", async (route) => {
    const req = route.request();
    const path = new URL(req.url()).pathname.replace("/api/v1/", "/api/");
    if (path === "/api/config" && req.method() === "GET") {
      return route.fulfill({ json: stored });
    }
    if (path === "/api/config" && req.method() === "PUT") {
      stored = req.postDataJSON();
      writes.push(stored);
      irrigation.restart_required = true;
      irrigation.restart_reasons = ["Weather source connections changed."];
      return route.fulfill({ json: { saved: 1, restart_required: true,
        restart_reasons: irrigation.restart_reasons } });
    }
    // Keep the authoritative runtime notice and catalog in the same fixture
    // as config. The CI host may retain a real wizard restart hold of its own.
    if (path === "/api/irrigation/snapshot" && req.method() === "GET") {
      return route.fulfill({ json: irrigation });
    }
    if (path === "/api/irrigation/stream" && req.method() === "GET") {
      return route.fulfill({ contentType: "text/event-stream",
        body: `event: snapshot\ndata: ${JSON.stringify(irrigation)}\n\n` });
    }
    if (path === "/api/config/source_catalog" && req.method() === "GET") {
      return route.fulfill({ json: { lat: 51.5, lon: -0.1, cloud_sources: [] } });
    }
    if (path === "/api/devices" && req.method() === "GET") {
      return route.fulfill({ json: stored.sources.map((s: any) => ({
        id: `source:${s.id}`, source_id: s.id, source_kind: s.kind,
        name: s.id === "fixture_ha" ? "Fixture HA" : "Fixture Tempest",
        kind: s.kind === "ha_passthrough" ? "ha_bridge" : "weather_gateway",
        origin: "native", online: true, enabled: s.enabled, children: [],
      })) });
    }
    if (!["GET", "HEAD"].includes(req.method())) {
      unexpected.push(`${req.method()} ${path}`);
      return route.fulfill({ status: 409, json: { error: "Unexpected fixture write" } });
    }
    return route.continue();
  });
  return { writes, unexpected };
}

test("HA optional weather mappings save and reload with honest restart and rain guidance", async ({ page }, testInfo) => {
  const { writes, unexpected } = await sourceFixture(page, {
    id: "fixture_ha", kind: "ha_passthrough", enabled: true, priority: 50,
    config: { base_url: "http://192.0.2.10:8123", bearer_token: "fixture-only",
      field_map: { air_temp_f: "sensor.weatherflow_temperature" } },
  });
  await open(page, "/settings?section=devices");
  await page.getByText("Fixture HA", { exact: true }).click();
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await expect(page.getByText(/For HA WeatherFlow precipitation, choose Rain last minute/)).toBeVisible();
  const guide = page.getByRole("link", { name: "WeatherFlow setup guide" });
  const response = await page.request.get((await guide.getAttribute("href"))!);
  expect(response.ok()).toBeTruthy();
  expect(await response.text()).toContain('id="keeping-weatherflow-in-home-assistant"');
  const additions = {
    RainLastMinIn: "sensor.weatherflow_precipitation",
    illuminance: "sensor.weatherflow_illuminance",
    lightning_count: "sensor.weatherflow_lightning_count",
    lightning_distance_mi: "sensor.weatherflow_lightning_distance",
  };
  for (const [field, entity] of Object.entries(additions)) {
    await page.getByRole("button", { name: "+ Add a field mapping", exact: true }).click();
    const row = page.locator(".soil-sub-card").last();
    await row.getByRole("combobox", { name: "Reading", exact: true }).selectOption(field);
    const fields = await page.locator(".soil-sub-card select").evaluateAll(nodes => nodes.map(n => (n as HTMLSelectElement).value));
    const index = fields.indexOf(field);
    expect(index).toBeGreaterThanOrEqual(0);
    await page.locator(".soil-sub-card").nth(index)
      .getByRole("textbox", { name: "HA entity id", exact: true }).fill(entity);
  }
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await expect(guide).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath(`ha-mappings-${width}.png`), fullPage: true });
  }
  await page.getByRole("button", { name: "Save changes", exact: true }).click();
  await expect.poll(() => writes.length).toBe(1);
  expect(writes[0].sources[0].config.field_map).toEqual({
    air_temp_f: "sensor.weatherflow_temperature", ...additions,
  });
  await expect(page.getByText("Saved. Restart LocalSky to use it.", { exact: true })).toBeVisible();
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
  await page.getByText("Fixture HA", { exact: true }).click();
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  for (const [field, entity] of Object.entries(additions)) {
    // The saved map is sorted by field when it is reloaded.
    const fields = await page.locator(".soil-sub-card select").evaluateAll(nodes => nodes.map(n => (n as HTMLSelectElement).value));
    const index = fields.indexOf(field);
    expect(index).toBeGreaterThanOrEqual(0);
    await expect(page.locator(".soil-sub-card").nth(index).getByRole("textbox", { name: "HA entity id", exact: true })).toHaveValue(entity);
  }
  expect(unexpected).toEqual([]);
});

for (const action of ["disable", "remove"] as const) {
  test(`Tempest ${action} confirmation preserves the server restart requirement`, async ({ page }) => {
    const { writes, unexpected } = await sourceFixture(page, {
      id: "fixture_tempest", kind: "tempest_udp", enabled: true, priority: 100,
      config: { bind_addr: "127.0.0.1:50222" },
    });
    await open(page, "/settings?section=devices");
    await page.getByText("Fixture Tempest", { exact: true }).click();
    if (action === "remove") {
      await page.getByRole("button", { name: "Remove", exact: true }).click();
      await page.getByRole("dialog", { name: "Remove this source?" })
        .getByRole("button", { name: "Remove source", exact: true }).click();
    } else {
      await page.locator("li.settings-card-list__item")
        .filter({ has: page.getByText("Fixture Tempest", { exact: true }) })
        .getByRole("switch").click();
    }
    await expect.poll(() => writes.length).toBe(1);
    if (action === "remove") expect(writes[0].sources).toEqual([]);
    else expect(writes[0].sources[0].enabled).toBe(false);
    const result = action === "remove" ? "Source removed." : "Source turned off.";
    await expect(page.getByText(`${result} Restart LocalSky to finish applying this change.`, { exact: true })).toBeVisible();
    await expect(page.getByText("Weather source connections changed.", { exact: true })).toBeVisible();
    if (action === "disable") {
      await page.reload({ waitUntil: "domcontentloaded" });
      await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true");
      await page.getByText("Fixture Tempest", { exact: true }).click();
      const toggle = page.locator("li.settings-card-list__item")
        .filter({ has: page.getByText("Fixture Tempest", { exact: true }) })
        .getByRole("switch");
      await expect(toggle).toHaveAttribute("aria-checked", "false");
      expect(writes).toHaveLength(1); // loading saved state must not save again
      await toggle.click();
      await expect.poll(() => writes.length).toBe(2);
      expect(writes[1].sources[0].enabled).toBe(true);
      await expect(page.getByText("Source turned on. Restart LocalSky to finish applying this change.", { exact: true })).toBeVisible();
    }
    expect(unexpected).toEqual([]);
  });
}

for (const path of ["/settings/zones?edit=fixture_zone", "/settings?section=zones&edit=fixture_zone"]) {
  test(`zone fields survive delayed config and dynamic controls: ${path}`, async ({ page }) => {
    const writes = await fixture(page);
    await open(page, path);
    await expect(page.getByRole("textbox", { name: "Name", exact: true })).toHaveValue("Fixture lawn");
    const controllers = page.getByRole("radiogroup", { name: "Controller id", exact: true });
    await expect(controllers.getByRole("radio")).toHaveCount(2);
    await expect(controllers.getByRole("radio", { name: "fixture_a" })).toHaveAttribute("aria-checked", "true");

    // The field gains a select after a scan. Both native controls need a
    // name, including the input the previous document observer skipped.
    await page.getByRole("textbox", { name: "Controller station", exact: true }).click();
    const picker = page.getByRole("combobox", { name: "Controller station", exact: true });
    await expect(picker).toBeVisible();
    await picker.selectOption("fixture-station");
    await expect(page.getByRole("textbox", { name: "Controller station", exact: true })).toHaveValue("fixture-station");
    await controllers.getByRole("radio", { name: "fixture_b" }).click();
    await expect(page.getByRole("textbox", { name: "Controller station", exact: true })).toHaveValue("");
    await expect(picker).toHaveCount(0);
    expect(writes).toHaveLength(1); // the stubbed scan, never a save or run

    await page.locator('.ui-form-field__label').filter({ hasText: /^Name$/ }).click();
    await expect(page.getByRole("textbox", { name: "Name", exact: true })).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog", { name: "Discard this zone?" })).toBeVisible();
    await page.getByRole("dialog", { name: "Discard this zone?" }).getByRole("button", { name: "Discard", exact: true }).click();
    await expect(page.locator(".sidebar")).not.toHaveAttribute("inert", "");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
}

for (const [path, selector, kind] of [
  ["/irrigation", ".override-panel", "set_global_override"],
  ["/zones", ".override-ctl--compact", "set_zone_override"],
] as const) {
  test(`Force requires consequence confirmation: ${kind}`, async ({ page }) => {
    const writes = await fixture(page);
    await open(page, path);
    const control = page.locator(selector).first();
    await control.getByRole("button", { name: "Force", exact: true }).click();
    const confirmation = page.getByRole("dialog", { name: "Force scheduled watering?" });
    await expect(confirmation).toBeVisible();
    await expect(confirmation).toContainText("overwater plants and waste water");
    await expect(confirmation).toContainText("no valve starts now");
    // The SSE fixture reconnects with fresh snapshots. A card can rebuild
    // while its page-owned confirmation remains open.
    await page.waitForTimeout(1500);
    await expect(confirmation).toBeVisible();
    expect(writes).toEqual([]);
    await confirmation.getByRole("button", { name: "Cancel", exact: true }).click();
    expect(writes).toEqual([]);
    await control.getByRole("button", { name: "Force", exact: true }).click();
    await confirmation.getByRole("button", { name: "Enable Force", exact: true }).click();
    await expect.poll(() => writes.length).toBe(1);
    expect(writes[0]).toMatchObject({ kind, mode: "run" });
    await expect(control).toContainText("Force stays on until Auto");
  });
}


test("watering sessions group cycles and observations across midnight without inflating water", async ({ page }) => {
  const writes: string[] = [];
  const start = Date.parse("2026-09-09T23:30:00-04:00") / 1000;
  const row = (session_id: string | null, start_epoch: number, duration_s: number,
               source = "manual", cycle_index: number | null = null) => ({
    session_id, zone: "orchard", start_epoch, duration_s, source, cycle_index,
    cycle_count: cycle_index === null ? null : 2, status: "completed",
    controller_id: "fixture", skip_reason: null, note: null,
    applied_mm: null, volume_gal: null,
  });
  const runs = [
    row("session-a", start, 600, "manual", 0),
    row("session-a", start + 60, 480, "ha_refresher", 0),
    row("session-b", start + 900, 300),
    row("session-a", start + 10800, 600, "manual", 1),
    row(null, start + 45000, 60),
  ];
  const responses: Record<string, any> = visualDemo.responses;
  const streams: Record<string, string> = {
    "/api/stream": "/api/snapshot",
    "/api/irrigation/stream": "/api/irrigation/snapshot",
    "/api/forecast/stream": "/api/forecast/snapshot",
  };
  await page.clock.setFixedTime(new Date(visualDemo.now));
  await page.route("**/api/**", async route => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname.replace("/api/v1/", "/api/");
    if (request.method() !== "GET") {
      writes.push(path);
      return route.fulfill({ status: 405, json: {} });
    }
    if (path === "/api/irrigation/history") {
      return route.fulfill({ json: { from_epoch: start - 86400, to_epoch: start + 86400, runs } });
    }
    const key = path + url.search;
    const response = responses[streams[key] ?? key];
    if (response === undefined) return route.continue();
    if (streams[key]) return route.fulfill({
      contentType: "text/event-stream",
      body: "event: snapshot\ndata: " + JSON.stringify(response) + "\n\n",
    });
    return route.fulfill({ json: response });
  });
  await page.goto("/history", { waitUntil: "domcontentloaded" });
  const sessions = page.locator(".runlog-session");
  await expect(sessions).toHaveCount(2);
  const first = sessions.filter({ hasText: "2 cycles" });
  await expect(first).toHaveCount(1);
  await expect(first.locator("summary")).toContainText("20");
  await first.locator("summary").press("Enter");
  await expect(first).toHaveAttribute("open", "");
  await expect(first.locator(":scope > div.runlog-row")).toHaveCount(3);
  await expect(first).toContainText("cycle 1 of 2");
  await expect(first).toContainText("cycle 2 of 2");
  await expect(first).not.toContainText("cycle 3");
  const sessionTile = page.locator(".hist-kpis .stat-tile").filter({ hasText: "Watering sessions" });
  await expect(sessionTile.locator(".stat-tile__value")).toHaveText("3");
  const waterTile = page.locator(".hist-kpis .stat-tile").filter({ hasText: "Watering time" });
  await expect(waterTile.locator(".stat-tile__value")).toHaveText("26");
  expect(writes).toEqual([]);
});

test("restore bundle picker works from the keyboard and cancel does not upload", async ({ page }) => {
  const writes = await fixture(page);
  // A visible SSR button does not yet have its WASM event handler. Exercise a
  // cold load and wait for actual hydration before sending the keyboard action.
  await page.route("**/pkg/*.wasm", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 500));
    return route.continue();
  });
  await open(page, "/settings/advanced");
  const button = page.getByRole("button", { name: "Restore from bundle…" });
  await expect(button).toBeVisible();
  await button.focus();
  const chooserPromise = page.waitForEvent("filechooser");
  await button.press("Enter");
  const chooser = await chooserPromise;
  await chooser.setFiles({ name: "fixture-only.tar.gz", mimeType: "application/gzip",
    buffer: Buffer.from("fixture-only; no upload is authorized by this test") });
  const dialog = page.getByRole("dialog", { name: "Restore from this bundle?" });
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(dialog).toBeHidden();
  expect(writes).toEqual([]);
});


test("Wind distinguishes measured average from estimated fallback and keeps details open", async ({ page }, testInfo) => {
  let modeled = false;
  let empty = false;
  let deliveries = 0;
  const unexpected: string[] = [];
  await page.route("**/api/**", async route => {
    const req = route.request();
    const path = new URL(req.url()).pathname.replace("/api/v1/", "/api/");
    if (!["GET", "HEAD"].includes(req.method())) {
      unexpected.push(`${req.method()} ${path}`);
      return route.fulfill({ status: 409, json: { error: "No writes in wind review" } });
    }
    const responses = visualDemo.responses as Record<string, any>;
    if (["/api/irrigation/snapshot", "/api/irrigation/stream"].includes(path)) {
      const snap = structuredClone(responses["/api/irrigation/snapshot"]);
      snap.last_refresh_epoch = 1789089400;
      snap.current_weather = { wind_mph: {
        value: modeled ? 8.3 : 4.6, source_id: modeled ? "open_meteo" : "nws",
        observed_epoch: 1789089100, max_age_s: 2100, measured: !modeled,
        selection_reason: modeled ? "nws report expired; using fallback" : "Preferred source",
      } };
      if (empty) snap.current_weather = {};
      if (path.endsWith("stream")) {
        deliveries++;
        return route.fulfill({ contentType: "text/event-stream", body: `event: snapshot\ndata: ${JSON.stringify(snap)}\n\n` });
      }
      return route.fulfill({ json: snap });
    }
    if (responses[path]) return route.fulfill({ json: responses[path] });
    return route.continue();
  });
  await open(page, "/");
  const detail = page.locator("details.wind-evidence");
  await expect(detail.locator("summary")).toHaveText("Average: nws · measured · 5m old");
  await expect(page.locator(".wind-bar-live")).toHaveCount(0);
  await expect(page.locator(".wind-bar-hot .wind-bar-value")).toHaveText("—");
  await expect(page.locator(".compass")).toHaveAttribute("aria-label", "Wind direction unavailable");
  await detail.locator("summary").click();
  await expect(detail).toHaveAttribute("open", "");
  await expect(detail.getByText(/Preferred source/)).toBeVisible();
  const before = deliveries;
  modeled = true;
  await expect.poll(() => deliveries, { timeout: 15000 }).toBeGreaterThan(before);
  await expect(detail.locator("summary")).toHaveText("Average: open_meteo · estimated · 5m old");
  await expect(detail).toHaveAttribute("open", "");
  await expect(detail.getByText(/nws report expired; using fallback/)).toBeVisible();
  for (const width of [1280, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath(`wind-source-${width}.png`), fullPage: true });
  }
  // A current response with no accepted wind is different from an old API
  // that predates provenance. A rain-only HA source must not paint calm wind.
  const previous = deliveries;
  empty = true;
  await expect.poll(() => deliveries, { timeout: 15000 }).toBeGreaterThan(previous);
  await expect(page.locator(".wind-bar-mid .wind-bar-value")).toHaveText("—");
  await expect(page.locator(".wind-bar-live")).toHaveCount(0);
  expect(unexpected).toEqual([]);
});

for (const width of [1280, 390]) {
  test(`failed irrigation action keeps copyable server evidence at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await fixture(page);
    await page.route("**/api/irrigation/action", route => route.fulfill({ status: 502, json: {
      error: "Controller request failed", request: { id: "fixture-request-092", method: "POST", route: "/api/irrigation/action" },
      diagnostic: { at_epoch: 1790000000, failure: { code: "LS_HTTP_SERVER", operation: "controller request", http_status: 503,
        message: "upstream returned a server error", next_step: "Check controller logs at this timestamp." } },
    } }));
    await open(page, "/zones/fixture_zone");
    await page.getByRole("button", { name: "Run now", exact: true }).click();
    const toast = page.locator(".ui-toast--error").filter({ hasText: "Zone command failed" });
    await expect(toast).toBeVisible();
    const details = toast.locator("details.diagnostic-details");
    await expect(details).not.toHaveAttribute("open", "");
    await details.locator("summary").click();
    await expect(details.locator("pre")).toContainText("fixture-request-092");
    await expect(details.locator("pre")).toContainText('"http_status": 503');
    await details.getByRole("button", { name: "Copy details", exact: true }).click();
    await expect(details.getByRole("button")).toHaveText(/Copied|Select and copy below/);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBeTruthy();
    expect(errors).toEqual([]);
  });
}
