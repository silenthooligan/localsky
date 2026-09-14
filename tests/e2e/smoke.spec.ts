import { test, expect, Page, ConsoleMessage } from "@playwright/test";
import visualDemo from "./fixtures/visual-demo.json";

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

    const resp = await page.goto(p.path, { waitUntil: "domcontentloaded" });

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

}

// The live render tests above exercise the candidate's actual API. Visual
// comparisons use a separate, fixed demo scenario: the demo feeder's clock,
// weather cycle, asynchronously seeded history, and host fonts otherwise move
// thousands of pixels between identical builds. Review expected/actual/diff
// images before deliberately replacing a baseline; a failure is not permission
// to accept it. The Rust demo regression checks the producer's Today/slot parity.
for (const screen of [
  { name: "desktop", width: 1280, height: 720 },
  { name: "phone", width: 390, height: 844 },
]) {
test.describe("fixed demo visuals " + screen.name, () => {
  test.use({
    serviceWorkers: "block",
    viewport: { width: screen.width, height: screen.height },
    timezoneId: "America/New_York",
    locale: "en-US",
  });

  for (const p of PAGES) {
    test(`${p.name} (${p.path}) matches its visual baseline`, async ({ page }) => {
      const { errors } = watchErrors(page);
      const unexpected: string[] = [];
      const served = new Set<string>();
      const responses: Record<string, unknown> = visualDemo.responses;
      // Freeze external map artwork for visual comparisons. The radar suite
      // separately checks real tile configuration and layer interactions.
      await page.route("**/*", async route => {
        const request = route.request();
        if (request.resourceType() === "image"
            && new URL(request.url()).origin !== new URL(page.url()).origin) {
          return route.fulfill({ contentType: "image/png",
            body: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j1ioAAAAASUVORK5CYII=", "base64") });
        }
        return route.fallback();
      });
      const streams: Record<string, string> = {
        "/api/stream": "/api/snapshot",
        "/api/irrigation/stream": "/api/irrigation/snapshot",
        "/api/forecast/stream": "/api/forecast/snapshot",
      };
      await page.clock.setFixedTime(new Date(visualDemo.now));
      await page.route("**/api/**", async (route) => {
        const request = route.request();
        const url = new URL(request.url());
        const key = url.pathname.replace("/api/v1/", "/api/") + url.search;
        const response = responses[streams[key] ?? (key === "/api/irrigation/history?days=2" ? "/api/irrigation/history?days=30" : key)];
        // Every API request is isolated, including writes. Unknown reads fail
        // visibly instead of borrowing changing data from the target instance.
        if (request.method() !== "GET" || response === undefined) {
          unexpected.push(`${request.method()} ${key}`);
          return route.fulfill({ status: 501, json: { error: "Missing visual fixture" } });
        }
        served.add(key);
        if (streams[key]) {
          return route.fulfill({
            contentType: "text/event-stream",
            body: `event: snapshot\ndata: ${JSON.stringify(response)}\n\n`,
          });
        }
        return route.fulfill({ json: response });
      });

      const response = await page.goto(p.path, { waitUntil: "domcontentloaded" });
      expect(response?.ok(), `SSR document for ${p.path}`).toBe(true);
      await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
      await expect(page.locator("html")).toHaveAttribute("data-demo", "true");
      await expect.poll(() => [
        ...Object.keys(streams), "/api/info", "/api/health", "/api/irrigation/tuning",
      ].every((key) => served.has(key))).toBe(true);

      // CI installs DejaVu. Explicit families avoid selecting an optional Inter,
      // Segoe UI, or system monospace font on another acceptance host.
      await page.addStyleTag({ content: `:root {
        --font-sans: "DejaVu Sans", sans-serif;
        --font-display: "DejaVu Sans", sans-serif;
        --font-mono: "DejaVu Sans Mono", monospace;
      }` });
      await page.evaluate(() => document.fonts.ready);
      // Allow the compositor to repaint fixed translucent navigation after the
      // explicit font swap. Geometry can settle before its cached layer does.
      await page.waitForTimeout(500);

      if (p.name === "irrigation") {
        const hero = page.locator(".next-run-hero");
        await expect(hero.locator(".irrigation-overview__today")).toBeVisible();
        // This fixed scenario has unconfirmed controller state. A recorded
        // morning cannot mask that current status, while tomorrow stays a projection.
        await expect(hero.locator(".next-run-headline")).toHaveText("Checking valves");
        await expect(hero.locator(".irrigation-overview__tomorrow > strong")).toHaveText("Not watering");
        await expect(page.locator(".water-plan")).toHaveCount(0);
        // The hero must keep its decision and reason inside the card across
        // desktop and phone layouts. This catches the real clip
        // independently of the screenshot's page-wide pixel allowance.
        const clipped = await hero.evaluate((card) => {
          const bounds = card.getBoundingClientRect();
          return [...card.querySelectorAll(".next-run-headline, .next-run-tag, .irrigation-overview__tomorrow p")]
            .filter((element) => {
              const range = document.createRange();
              range.selectNodeContents(element);
              return [...range.getClientRects()].some((rect) =>
                rect.left < bounds.left || rect.right > bounds.right
                || rect.top < bounds.top || rect.bottom > bounds.bottom);
            }).map((element) => element.textContent);
        });
        expect(clipped, "hero text clipped by its card").toEqual([]);
      }

      const layout = await page.evaluate(() => {
        const outside = [...document.querySelectorAll(".mobile-tab-bar > *")]
          .filter(e => { const r = e.getBoundingClientRect(); return r.width > 0 && (r.left < 0 || r.right > innerWidth + 1); })
          .map(e => e.textContent);
        const clippedStats = [...document.querySelectorAll(".zones-kpis .stat-tile")]
          .filter(tile => {
            const bounds = tile.getBoundingClientRect();
            return [...tile.querySelectorAll(".stat-tile__label,.stat-tile__value,.stat-tile__unit")]
              .some(e => { const range = document.createRange(); range.selectNodeContents(e);
                return [...range.getClientRects()].some(r => r.left < bounds.left || r.right > bounds.right); });
          }).map(e => e.textContent);
        return { pageOverflow: document.documentElement.scrollWidth > innerWidth, outside, clippedStats };
      });
      expect(layout).toEqual({ pageOverflow: false, outside: [], clippedStats: [] });

      expect(unexpected, "unhandled visual fixture requests").toEqual([]);
      expect(errors, `console/page errors on ${p.path}`).toEqual([]);
      await page.screenshot({ path: test.info().outputPath(`${screen.name}-${p.name}-review.png`), animations: "disabled" });
      await expect(page).toHaveScreenshot((screen.name === "phone" ? "phone-" : "") + `${p.name}.png`, {
        maxDiffPixelRatio: 0.03,
        animations: "disabled",
      });
      expect(unexpected, "unhandled visual fixture requests").toEqual([]);
      expect(errors, `console/page errors on ${p.path}`).toEqual([]);
    });
  }
});
}
