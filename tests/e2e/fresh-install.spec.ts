import { test, expect, Page } from "@playwright/test";

// The first hour, against an EMPTY volume. This suite assumes the target is
// a LocalSky with no localsky.toml yet (the pre-merge job boots the freshly
// built image on a tmpfs /data). It is sequential by design: one install,
// one wizard, one zone edit, in order.
//
//   FRESH_INSTALL=1 BASE_URL=http://localhost:18090 npx playwright test fresh-install.spec.ts
//
// Skipped without FRESH_INSTALL=1 so the daily canary against the demo
// (which has a config) never runs it.

test.describe.configure({ mode: "serial" });

const fresh = process.env.FRESH_INSTALL === "1";

// The app sets data-hydrated on <html> once the WASM has attached its
// handlers; typing before that goes into a page nobody is listening to.
async function hydrated(page: Page) {
  await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true", null, {
    timeout: 30_000,
  });
}

async function open(page: Page, path: string) {
  await page.goto(path, { waitUntil: "networkidle" });
  await hydrated(page);
}

async function next(page: Page) {
  await page.getByRole("link", { name: "Next", exact: true }).click();
  await hydrated(page);
}

test.skip(!fresh, "needs an empty-volume instance (FRESH_INSTALL=1)");

test("an unconfigured install sends the front door to /setup", async ({ page, request }) => {
  const head = await request.get("/", { maxRedirects: 0 });
  expect(head.status()).toBe(302);
  expect(head.headers()["location"]).toBe("/setup");
  const health = await (await request.get("/api/v1/health")).json();
  expect(health.config_present).toBe(false);
  expect(health.location_configured).toBe(false);
  await page.goto("/");
  await expect(page).toHaveURL(/\/setup/);
});

test("the wizard's happy path writes a config with a rule and a zone", async ({ page, request }) => {
  test.setTimeout(120_000);
  await open(page, "/setup/welcome");
  await next(page);

  // Location: both typed coordinates must reach the saved draft before Next.
  await expect(page).toHaveURL(/\/setup\/location/);
  const lat = page.getByPlaceholder("e.g. 40.7128");
  const lon = page.getByPlaceholder("e.g. -74.0060");
  await lat.fill("29.65");
  await lat.blur();
  await lon.fill("-82.32");
  await lon.blur();
  await expect(page.getByRole("link", { name: "Next", exact: true })).toBeVisible({ timeout: 15_000 });
  const locationDraft = await (await request.get("/api/v1/wizard/draft")).json();
  expect(locationDraft.config.deployment.location.lat).toBeCloseTo(29.65, 6);
  expect(locationDraft.config.deployment.location.lon).toBeCloseTo(-82.32, 6);
  await next(page);

  // Sources: the seeded forecast sources are enough.
  await expect(page).toHaveURL(/\/setup\/sources/);
  await next(page);

  // Controller: no hardware, simulate.
  await expect(page).toHaveURL(/\/setup\/controllers/);
  await page.getByRole("button", { name: /simulate/i }).click();
  await expect(page.getByText(/dry[- ]run|simulat/i).first()).toBeVisible();
  await next(page);

  // Zones: one zone on the simulated controller.
  await expect(page).toHaveURL(/\/setup\/zones/);
  await page.getByRole("button", { name: /Add your first zone/ }).click();
  await page.getByPlaceholder("Back Yard").fill("Front Lawn");
  await page.getByRole("button", { name: "Add zone", exact: true }).click();
  await expect(page.getByText("Front Lawn")).toBeVisible();
  await next(page);

  // Rules: the two-days-a-week starter.
  await expect(page).toHaveURL(/\/setup\/rules/);
  await page.getByRole("radio", { name: /Two days a week/ }).click();
  await next(page);

  // The optional steps: nothing to add.
  for (const step of ["sensors", "llm", "notifications", "account"]) {
    await expect(page).toHaveURL(new RegExp(`/setup/${step}`));
    await next(page);
  }

  // Review and apply. This install has been running with NO configuration at
  // all, so nothing it just wrote is wired in the live process: the controller
  // registry is empty and no source adapter was ever spawned, and neither is
  // rebuilt after boot. The apply says so, and the step HOLDS instead of
  // declaring the yard configured. That is the assertion that matters here:
  // the defect this replaced redirected to the dashboard every single time, so
  // pinning "we left /setup/review" (which the old bug satisfied) proved
  // nothing about the promise the page makes.
  await expect(page).toHaveURL(/\/setup\/review/);
  await expect(page.getByText("Front Lawn")).toBeVisible();
  await expect(page.getByText(/Two days a week/)).toBeVisible();
  await page.getByRole("button", { name: /Save and finish/ }).click();

  const banner = page.locator(".health-banner", { hasText: "Restart required to apply." });
  await expect(banner).toBeVisible({ timeout: 30_000 });
  await expect(page).toHaveURL(/\/setup\/review/);
  // The step's own record of the hold, which no dismissal can take away.
  await expect(page.getByText(/Saved, and not running yet/)).toBeVisible();
  // And the reasons name the wiring the running process does not have.
  await expect(banner).toContainText(/controllers are not connected/i);
  await expect(banner).toContainText(/weather sources are not connected/i);

  // Restart now: the button that copy points at, through to the confirm.
  // It stops AT the confirm on purpose. The pre-merge harness starts the app
  // with `docker run` and no restart policy (tests/e2e/pre-merge.sh), so the
  // process exiting IS the end of the container: confirming here would take
  // the rest of this file, and all of axe.spec.ts against the same instance,
  // down with it. Exercising the button and its confirmation is what proves
  // the hold hands the owner a way out.
  await banner.getByRole("button", { name: "Restart LocalSky now" }).click();
  const confirm = page.locator(".sheet--open");
  await expect(confirm.getByText("Restart LocalSky now?")).toBeVisible();
  await expect(confirm.getByRole("button", { name: "Restart now", exact: true })).toBeVisible();
  await confirm.getByRole("button", { name: "Cancel", exact: true }).click();

  // The config exists, and says what the wizard was told.
  const health = await (await request.get("/api/v1/health")).json();
  expect(health.config_present).toBe(true);
  expect(health.location_configured).toBe(true);
  const cfg = await (await request.get("/api/v1/config")).json();
  expect(cfg.deployment.location.lat).toBeCloseTo(29.65, 6);
  expect(cfg.deployment.location.lon).toBeCloseTo(-82.32, 6);
  expect(cfg.deployment.timezone).toBe("America/New_York");
  expect(cfg.deployment.units).toBe("imperial");
  expect(Object.keys(cfg.zones)).toContain("front_lawn");
  expect(cfg.engine.watering_restrictions.some((r: any) => r.allowed_weekdays?.length === 2)).toBe(true);

  // And the front door is the dashboard now, not the wizard. Note this says
  // the CONFIG exists, not that the yard can water: that part is what the
  // restart above is still owed for.
  const head = await request.get("/", { maxRedirects: 0 });
  expect(head.status()).toBe(200);
});

test("a zone edit from settings persists", async ({ page, request }) => {
  test.setTimeout(60_000);
  await open(page, "/settings/zones");
  // The zone card is collapsed; its header opens the actions.
  await page.getByRole("button", { name: /Front Lawn/, expanded: false }).first().click();
  await page.getByRole("button", { name: "Edit zone front_lawn" }).click();
  // Editing opens the drawer now, not a form spliced into the list under
  // the card. The title names the zone so it is clear what is being edited
  // once the form is no longer physically attached to its row.
  const drawer = page.locator(".sheet--drawer");
  await expect(drawer).toHaveClass(/sheet--open/);
  await expect(drawer.getByText("Editing Front Lawn")).toBeVisible();
  const name = page.getByPlaceholder("Back Yard");
  await expect(name).toHaveValue("Front Lawn");
  await name.fill("Front Lawn (edited)");
  await page.getByRole("button", { name: "Save zone changes", exact: true }).click();
  await expect(page.locator(".settings-card__title", { hasText: "Front Lawn (edited)" })).toBeVisible({
    timeout: 15_000,
  });
  const cfg = await (await request.get("/api/v1/config")).json();
  expect(cfg.zones.front_lawn.display_name).toBe("Front Lawn (edited)");
});
