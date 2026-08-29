import { test, expect } from "@playwright/test";

// Issue #8's other half, end to end in the real UI: bind a zone to one of the
// controller's own zones WITHOUT renaming anything on either side.
//
// The reporter got his Rachio zones running by renaming them in the Rachio
// app until the slugified vendor names matched his LocalSky slugs, because
// the only binding that existed was a map keyed by the vendor's names. This
// spec seeds exactly that mismatch (a Rachio zone called "Front Lawn"
// against a LocalSky zone slugged front_yard) and drives the picker: the
// options carry the CONTROLLER's names, saving writes the uuid into
// zones.front_yard.controller_station, and the zone's key never moves.
//
// Interaction-only (no screenshot baselines). Everything the flow mutates is
// stubbed with page.route so it runs against the read-only demo instance. The
// entire client is real: seeding, the reactive kind gate, the scan request,
// the option list, the save payload, and the editor re-seed after reload.

// The app registers a service worker; route interception must see every
// fetch, so keep workers out of this context.
test.use({ serviceWorkers: "block" });

const FRONT_LAWN = "1f00aa00-0000-4000-8000-0000000000a1";
const BACK_LAWN = "1f00aa00-0000-4000-8000-0000000000a2";

const SCAN_ZONES = [
  { station_id: FRONT_LAWN, name: "Front Lawn" },
  { station_id: BACK_LAWN, name: "Back Lawn" },
];

// A config with the names deliberately mismatched across the two sides, and
// no binding at all on the zone: the state that used to validate clean and
// silently never water.
function seedConfig() {
  return {
    schema_version: 1,
    deployment: { display_name: "Home", location: { lat: 28.5, lon: -81.4 } },
    sources: [],
    controllers: [
      {
        id: "rachio_main",
        default: true,
        enabled: true,
        kind: "rachio",
        config: {
          api_token: "***redacted***",
          device_id: "device-0001",
          zone_uuid_map: {},
        },
      },
    ],
    zones: {
      front_yard: {
        display_name: "Front Yard",
        area_sqft: 1000.0,
        species: "st_augustine",
        soil_texture: "sandy_loam",
        sprinkler_type: "rotor",
        controller_id: "rachio_main",
        controller_station: "",
      },
    },
  };
}

test("a zone binds by picking the controller's zone, and its slug never moves", async ({
  page,
}) => {
  let storedConfig: any = seedConfig();
  let putBody: any = null;
  let scanCalls = 0;

  await page.route("**/api/config", async (route) => {
    const req = route.request();
    if (req.method() === "PUT") {
      putBody = req.postDataJSON();
      storedConfig = putBody;
      await route.fulfill({ json: { ok: true, restart_required: false } });
    } else if (req.method() === "GET") {
      await route.fulfill({ json: storedConfig });
    } else {
      await route.continue();
    }
  });
  await page.route("**/api/v1/wizard/scan_zones", async (route) => {
    scanCalls += 1;
    await route.fulfill({ json: { zones: SCAN_ZONES } });
  });

  await page.goto("/settings/zones?edit=front_yard", {
    waitUntil: "networkidle",
  });

  // The zone is bound to nothing, and the UI says so before a run has to
  // fail to reveal it.
  await expect(page.getByText("Unbound").first()).toBeVisible();

  const station = page.locator(
    '.ui-form-field:has(label:text-is("Controller station")) input',
  );
  await expect(station).toHaveValue("");

  // The scan is LAZY: nothing has been asked of the controller yet, because
  // a scan is a live vendor request against a daily budget.
  expect(scanCalls).toBe(0);

  // First touch of the field is what asks.
  await station.click();
  await expect(
    page.locator(
      '.ui-form-field:has(label:text-is("Controller station")) select',
    ),
  ).toBeVisible();
  expect(scanCalls).toBe(1);

  // The options carry the CONTROLLER's names, not this zone's name.
  const picker = page.locator(
    '.ui-form-field:has(label:text-is("Controller station")) select',
  );
  await expect(picker.locator("option")).toContainText([
    "(not bound)",
    "Front Lawn",
    "Back Lawn",
  ]);

  await picker.selectOption(FRONT_LAWN);
  await expect(station).toHaveValue(FRONT_LAWN);
  // The binding reads back as an identity fact, by the controller's name.
  await expect(page.getByText(/rachio_main .* Front Lawn/)).toBeVisible();

  await page.getByRole("button", { name: "Save zone changes" }).click();
  await expect(page.getByText(/Saved\./)).toBeVisible();

  // The uuid landed on the zone, under the zone's ORIGINAL key. A rename
  // would have orphaned this zone's history, its Home Assistant entities and
  // its retained MQTT topics.
  expect(putBody).not.toBeNull();
  expect(Object.keys(putBody.zones)).toEqual(["front_yard"]);
  expect(putBody.zones.front_yard.controller_station).toBe(FRONT_LAWN);
  expect(putBody.zones.front_yard.controller_zone_name).toBe("Front Lawn");
  expect(putBody.zones.front_yard.display_name).toBe("Front Yard");

  // After a reload the card shows what it fires, and no longer says Unbound.
  await page.reload({ waitUntil: "networkidle" });
  await expect(page.getByText("Unbound")).toHaveCount(0);
});

test("a save made while the controller cannot be reached keeps the binding", async ({
  page,
}) => {
  // The dangerous case: the form writes the station field on EVERY save, so
  // a failed scan must not turn an unrelated edit into an unbind.
  let storedConfig: any = seedConfig();
  storedConfig.zones.front_yard.controller_station = FRONT_LAWN;
  storedConfig.zones.front_yard.controller_zone_name = "Front Lawn";
  let putBody: any = null;

  await page.route("**/api/config", async (route) => {
    const req = route.request();
    if (req.method() === "PUT") {
      putBody = req.postDataJSON();
      storedConfig = putBody;
      await route.fulfill({ json: { ok: true, restart_required: false } });
    } else if (req.method() === "GET") {
      await route.fulfill({ json: storedConfig });
    } else {
      await route.continue();
    }
  });
  await page.route("**/api/v1/wizard/scan_zones", async (route) => {
    await route.fulfill({
      status: 502,
      json: { error: "zone_scan_failed", detail: "controller offline" },
    });
  });

  await page.goto("/settings/zones?edit=front_yard", {
    waitUntil: "networkidle",
  });

  const station = page.locator(
    '.ui-form-field:has(label:text-is("Controller station")) input',
  );
  await expect(station).toHaveValue(FRONT_LAWN);

  // Touching the field asks and fails. The field stays a text box, says why,
  // and keeps its value.
  await station.click();
  await expect(page.getByText(/Could not list this controller's zones/)).toBeVisible();
  await expect(
    page.locator(
      '.ui-form-field:has(label:text-is("Controller station")) select',
    ),
  ).toHaveCount(0);
  await expect(station).toHaveValue(FRONT_LAWN);

  // Change something unrelated and save.
  await page
    .locator('.ui-form-field:has(label:text-is("Area (sq ft)")) input')
    .fill("1500");
  await page.getByRole("button", { name: "Save zone changes" }).click();
  await expect(page.getByText(/Saved\./)).toBeVisible();

  expect(putBody.zones.front_yard.area_sqft).toBe(1500);
  expect(putBody.zones.front_yard.controller_station).toBe(FRONT_LAWN);
});
