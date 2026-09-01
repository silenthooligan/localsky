import { test, expect, Page, ConsoleMessage } from "@playwright/test";

// The five pages a real user lives in. If any of these renders blank, throws a
// hydration/panic error in the console, or 404s, that is a regression the
// "cargo leptos build" compile gate cannot see. This suite is that missing net.
const PAGES = [
  { name: "weather-home", path: "/" },
  { name: "irrigation", path: "/irrigation" },
  { name: "zones", path: "/zones" },
  { name: "history", path: "/history" },
  { name: "settings", path: "/settings" },
] as const;

// Console noise that is not a real defect. Keep this list SHORT and specific;
// the whole point is to notice new console errors, so resist broadening it.
const IGNORED_CONSOLE = [
  /favicon\.ico/i,
  /manifest\.json.*(404|Failed to load)/i,
  // Service worker registration can warn on http/localhost; not a page defect.
  /ServiceWorker|service worker/i,
];

function isRealConsoleError(msg: ConsoleMessage): boolean {
  if (msg.type() !== "error") return false;
  const text = msg.text();
  return !IGNORED_CONSOLE.some((re) => re.test(text));
}

// Collect console.error + uncaught page errors (WASM panics surface here as
// "unreachable executed" / a PanicError) for the duration of one navigation.
function watchErrors(page: Page): { errors: string[] } {
  const errors: string[] = [];
  page.on("console", (msg) => {
    if (isRealConsoleError(msg)) errors.push(`console.error: ${msg.text()}`);
  });
  page.on("pageerror", (err) => {
    errors.push(`pageerror: ${err.message}`);
  });
  return { errors };
}

for (const p of PAGES) {
  test(`${p.name} (${p.path}) renders without errors`, async ({ page }) => {
    const { errors } = watchErrors(page);

    const resp = await page.goto(p.path, { waitUntil: "networkidle" });

    // The HTML document itself must load 2xx (SSR shell).
    expect(resp, `no response for ${p.path}`).not.toBeNull();
    expect(resp!.status(), `HTTP status for ${p.path}`).toBeLessThan(400);

    // The app root must actually contain rendered content, not an empty shell
    // (the classic "compiles but hydrates blank" failure).
    const body = page.locator("body");
    await expect(body).toBeVisible();
    const text = (await body.innerText()).trim();
    expect(text.length, `visible text on ${p.path}`).toBeGreaterThan(0);

    // The persistent primary navigation is present on every real page but
    // absent on the NotFound fallback, so this doubles as a "not 404" check.
    // Desktop renders it as the "Primary navigation" sidebar (an aside
    // landmark, not <nav>); mobile renders the "Primary mobile" tab bar and
    // hides the sidebar, so assert whichever landmark the viewport shows.
    // A bare locator("nav") resolved to the CSS-hidden mobile bar on the
    // 1280px viewport and could never pass.
    await expect(
      page
        .locator('[aria-label="Primary navigation"], [aria-label="Primary mobile"]')
        .filter({ visible: true })
        .first(),
      `primary navigation missing on ${p.path} (routed to NotFound?)`,
    ).toBeVisible();

    // Give hydration a beat to attach and surface any deferred panic.
    await page.waitForTimeout(500);

    expect(
      errors,
      `console/page errors on ${p.path}:\n${errors.join("\n")}`,
    ).toEqual([]);
  });

  // Layout drift lives in its OWN test on purpose. Folded into the test above,
  // a stale baseline failed the whole case, which made a cosmetic diff read
  // exactly like a blank page or a WASM panic. Any real UI change then turned
  // the canary red for weeks and taught us to skip past it. Split, the report
  // says which happened: "renders without errors" red means the page is
  // broken; "matches its visual baseline" red on its own means the baselines
  // are stale. To refresh, delete them, let the next run write and upload new
  // ones, and commit those to re-arm the diff.
  test(`${p.name} (${p.path}) matches its visual baseline`, async ({ page }) => {
    await page.goto(p.path, { waitUntil: "networkidle" });
    // Give hydration a beat so this captures the hydrated page.
    await page.waitForTimeout(500);

    // Viewport-only, not fullPage: live values change the page HEIGHT between
    // runs, and a dimension mismatch hard-fails before any pixel tolerance
    // applies. The fixed 1280x900 viewport keeps dimensions stable; the ratio
    // absorbs clock/value jitter so the baseline stays about LAYOUT.
    await expect(page).toHaveScreenshot(`${p.name}.png`, {
      maxDiffPixelRatio: 0.03,
      animations: "disabled",
    });
  });
}
