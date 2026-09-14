import { test, expect, Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

// The editor drawer, which is phase C2's own gate: axe clean while it is
// open, and a dirty form that is not thrown away by the keystroke that
// dismisses an empty one.
//
// Runs against any CONFIGURED instance, so it works against the demo and
// against the fresh install the pre-merge job has just walked through the
// wizard. It only adds a draft zone and discards it, so it leaves no
// state behind.
//
//   BASE_URL=http://localhost:18090 npx playwright test editor-drawer.spec.ts

async function hydrated(page: Page) {
  await page.waitForFunction(() => document.documentElement.dataset.hydrated === "true", null, {
    timeout: 30_000,
  });
}

async function openAddDrawer(page: Page) {
  // The form is URL-driven: ?add=1 is what the "+ Add zone" button
  // navigates to, so going straight there tests the same state without
  // depending on a button label.
  await page.goto("/settings/zones?add=1", { waitUntil: "networkidle" });
  await hydrated(page);
  const drawer = page.locator(".sheet--drawer");
  await expect(drawer).toHaveClass(/sheet--open/, { timeout: 15_000 });
  return drawer;
}

test("the add-a-zone editor opens as a drawer with no axe violations", async ({ page }) => {
  test.setTimeout(60_000);
  await openAddDrawer(page);
  // The dialog animates in; let it settle so axe measures the resting
  // state rather than a frame mid-transition.
  await page.waitForTimeout(600);
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
});

test("the page behind the open drawer is inert", async ({ page }) => {
  test.setTimeout(60_000);
  await openAddDrawer(page);
  // The viewport editor makes the background inert, either directly or
  // through a containing element. Both must prevent sidebar interaction.
  const sidebarInert = await page.locator(".sidebar").evaluate((el) => el.closest("[inert]") !== null);
  expect(sidebarInert).toBe(true);
});

// Every sheet in the app is always mounted and hidden with CSS, so
// presence in the DOM says nothing. Ask whether it is VISIBLE.
const discardDialog = (page: Page) => page.getByLabel("Discard this zone?");

test("Escape on a dirty editor asks before discarding", async ({ page }) => {
  test.setTimeout(60_000);
  await openAddDrawer(page);
  await page.getByPlaceholder("Back Yard").fill("Drawer Draft");
  await page.keyboard.press("Escape");

  // Asked, not closed.
  await expect(discardDialog(page)).toBeVisible();
  await expect(page.locator(".sheet--drawer")).toHaveClass(/sheet--open/);

  // Saying no leaves the draft exactly as typed.
  await discardDialog(page).getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(discardDialog(page)).not.toBeVisible();
  await expect(page.getByPlaceholder("Back Yard")).toHaveValue("Drawer Draft");

  // Saying yes closes it.
  await page.keyboard.press("Escape");
  await discardDialog(page).getByRole("button", { name: "Discard", exact: true }).click();
  await expect(page.locator(".sheet--drawer")).not.toHaveClass(/sheet--open/);
});

test("a clean editor closes on Escape without asking", async ({ page }) => {
  test.setTimeout(60_000);
  await openAddDrawer(page);
  await page.keyboard.press("Escape");
  await expect(discardDialog(page)).not.toBeVisible();
  await expect(page.locator(".sheet--drawer")).not.toHaveClass(/sheet--open/);
});

// The schedule and restriction editors moved out of the page and into the
// same drawer in the same phase. Neither had any coverage before, which is
// how a form panel spliced into a page survives three redesigns.
for (const [name, path, button] of [
  ["schedule", "/settings/schedules", "+ Add schedule"],
  ["restriction", "/settings/restrictions", "+ Add restriction"],
] as const) {
  test(`the ${name} editor opens as a drawer with no axe violations`, async ({ page }) => {
    test.setTimeout(60_000);
    await page.goto(path, { waitUntil: "networkidle" });
    await hydrated(page);
    await page.getByRole("button", { name: button, exact: true }).click();
    const drawer = page.locator(".sheet--drawer");
    await expect(drawer).toHaveClass(/sheet--open/, { timeout: 15_000 });
    await page.waitForTimeout(600);
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

    // Nothing typed, so it closes without asking.
    await page.keyboard.press("Escape");
    await expect(drawer).not.toHaveClass(/sheet--open/);
  });
}
