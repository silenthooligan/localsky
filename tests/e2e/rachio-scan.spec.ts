import { test, expect } from "@playwright/test";

// Issue #8 regression, end to end in the real UI: add a Rachio controller in
// Settings, scan its zones, and verify the scan RESULT lands in the Advanced
// JSON (zone_uuid_map), survives the two-step save, and is still there after
// a reload. The original defect: the scan reported "Found 7" while the JSON
// never changed and the save persisted an empty map.
//
// Interaction-only (no screenshot baselines). All API traffic the flow
// mutates is stubbed with page.route so the spec runs against the read-only
// demo instance: the scan endpoint answers a canned 7-zone Rachio response
// (example uuids), the config PUT is captured in-memory, and the config GET
// serves the captured save back after reload. What is REAL here is the
// entire client: picker, form, scan merge, textarea sync, commit, save
// payload, and the editor re-seed after reload.

// The app registers a service worker; route interception must see every
// fetch, so keep workers out of this context.
test.use({ serviceWorkers: "block" });

const SCAN_ZONES = [
  { station_id: "1f00aa00-0000-4000-8000-000000000001", name: "Front Lawn" },
  { station_id: "1f00aa00-0000-4000-8000-000000000002", name: "Back Lawn" },
  { station_id: "1f00aa00-0000-4000-8000-000000000003", name: "Side Beds" },
  { station_id: "1f00aa00-0000-4000-8000-000000000004", name: "Garden" },
  { station_id: "1f00aa00-0000-4000-8000-000000000005", name: "Driveway Strip" },
  { station_id: "1f00aa00-0000-4000-8000-000000000006", name: "Back Fence" },
  { station_id: "1f00aa00-0000-4000-8000-000000000007", name: "Front Trees" },
];

test("Rachio scan fills zone_uuid_map, saves, and persists across reload", async ({
  page,
}) => {
  // In-memory "server": GET /api/config serves what the last PUT saved.
  let storedConfig: unknown = { controllers: [], zones: {} };
  let putBody: any = null;

  await page.route("**/api/config", async (route) => {
    const req = route.request();
    if (req.method() === "PUT") {
      putBody = req.postDataJSON();
      storedConfig = putBody;
      await route.fulfill({ json: { ok: true } });
    } else if (req.method() === "GET") {
      await route.fulfill({ json: storedConfig });
    } else {
      await route.continue();
    }
  });
  await page.route("**/api/v1/wizard/scan_zones", async (route) => {
    await route.fulfill({ json: { zones: SCAN_ZONES } });
  });

  await page.goto("/settings/controllers", { waitUntil: "networkidle" });

  // Open the add form and pick the Rachio kind.
  await page.getByRole("button", { name: "+ Add controller" }).click();
  await page.getByRole("radio", { name: "Rachio", exact: true }).click();

  // Identity + connection fields. The token is the single password input;
  // the device id is the labeled text field of the Rachio connection form.
  await page.getByPlaceholder("e.g. os_main").fill("rachio_main");
  await page.locator('input[type="password"]').fill("example-api-token");
  await page
    .locator('.ui-form-field:has(label:text-is("Device ID")) input')
    .fill("device-0001");

  // Scan. The bug: this used to only set a message; the JSON never changed.
  await page
    .getByRole("button", {
      name: "Probe the controller and list its zones/stations (no save needed)",
    })
    .click();
  await expect(
    page.getByText("Found 7 zones and filled zone_uuid_map. Review and save."),
  ).toBeVisible();

  // Follow-through: a successful merge AUTO-OPENS the Advanced fold (no
  // manual click) and the filled JSON is visible where the user is
  // looking. The open attribute is the assertion, not a click.
  const fold = page.locator("details#controller-advanced-fold");
  await expect(fold).toHaveAttribute("open", "");
  const textarea = page.locator("textarea");
  await expect(textarea).toBeVisible();
  await expect(textarea).toHaveValue(/zone_uuid_map/);
  await expect(textarea).toHaveValue(/"front_lawn": "1f00aa00-0000-4000-8000-000000000001"/);
  await expect(textarea).toHaveValue(/"front_trees": "1f00aa00-0000-4000-8000-000000000007"/);

  // Two-step save: commit the entry, then save the whole config. Between
  // the steps the pending work is flagged: the Unsaved-changes chip shows
  // beside Save-all and clears once the save lands.
  await page.getByRole("button", { name: "Add controller", exact: true }).click();
  await expect(page.getByText("Unsaved changes")).toBeVisible();
  await page.getByRole("button", { name: "Save all changes" }).click();
  await expect(page.getByText(/Saved\./)).toBeVisible();
  await expect(page.getByText("Unsaved changes")).toHaveCount(0);

  // The persisted PUT body carries the full 7-entry map.
  expect(putBody).not.toBeNull();
  const saved = putBody.controllers.find((c: any) => c.id === "rachio_main");
  expect(saved).toBeTruthy();
  expect(saved.kind).toBe("rachio");
  expect(Object.keys(saved.config.zone_uuid_map)).toHaveLength(7);
  expect(saved.config.zone_uuid_map.side_beds).toBe(
    "1f00aa00-0000-4000-8000-000000000003",
  );

  // Reload: the page refetches the (captured) config; editing the entry
  // shows the persisted map in the Advanced JSON.
  await page.reload({ waitUntil: "networkidle" });
  await page.getByRole("button", { name: "Edit controller rachio_main" }).click();
  await page.getByText("Advanced: raw config JSON").click();
  const reopened = page.locator("textarea");
  await expect(reopened).toBeVisible();
  await expect(reopened).toHaveValue(/zone_uuid_map/);
  await expect(reopened).toHaveValue(/"garden": "1f00aa00-0000-4000-8000-000000000004"/);
  await expect(reopened).toHaveValue(/"back_fence": "1f00aa00-0000-4000-8000-000000000006"/);
});
