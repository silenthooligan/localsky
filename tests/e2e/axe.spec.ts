import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

// Accessibility on the five pages a person lives in, with axe-core. Runs
// against any configured instance (the demo, or the fresh install the
// pre-merge job just walked through the wizard). Zero violations is the
// bar: a violation here is a defect, not a score.
const PAGES = [
  { name: "weather-home", path: "/" },
  { name: "irrigation", path: "/irrigation" },
  { name: "zones", path: "/zones" },
  { name: "history", path: "/history" },
  { name: "settings", path: "/settings" },
] as const;

for (const p of PAGES) {
  test(`${p.name} (${p.path}) has no axe violations`, async ({ page }) => {
    await page.goto(p.path, { waitUntil: "domcontentloaded" });
    await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
    // Radar/provider traffic need not become idle for an accessible page.
    // Let the deferred client fetches settle so the real content is what
    // axe sees, not the skeletons.
    await page.waitForTimeout(1500);
    const results = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"])
      .analyze();
    const summary = results.violations.map((v) => ({
      id: v.id,
      impact: v.impact,
      help: v.help,
      nodes: v.nodes.slice(0, 5).map((n) => n.target.join(" ")),
    }));
    expect(summary, JSON.stringify(summary, null, 2)).toEqual([]);
    if (p.path === "/irrigation") {
      await page.goto("/irrigation/decisions", { waitUntil: "domcontentloaded" });
      await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
      await expect(page.getByRole("heading", { name: "Watering decisions", exact: true })).toBeVisible();
      // Match the other surfaces: measure settled content, not an intermediate
      // opacity in the route entrance or the first asynchronous history render.
      await page.waitForTimeout(1500);
      const decisions = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
      expect(decisions.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
    }
  });
}
