import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.use({ serviceWorkers: 'block', reducedMotion: 'reduce' });

test.describe('mobile More navigation', () => {
  test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });

  for (const motion of ['no-preference', 'reduce'] as const) {
    test(`About opens at the top after a long page (${motion})`, async ({ page }) => {
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.emulateMedia({ reducedMotion: motion });
      await appearance(page, 'field');
      await ready(page, '/');
      await expect(page.locator('#radar-map')).toHaveClass(/leaflet-container/);
      await page.evaluate(() => window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'instant' }));
      const previousScroll = await page.evaluate(() => scrollY);
      expect(previousScroll).toBeGreaterThan(100);
      const more = page.getByRole('button', { name: 'More', exact: true });
      const menu = page.getByRole('dialog', { name: 'More destinations' });

      // Dismissal stays on the current page and returns keyboard focus.
      await more.tap();
      await expect(menu.getByRole('button', { name: 'Close', exact: true })).toBeFocused();
      await page.keyboard.press('Escape');
      await expect(more).toBeFocused();
      expect(await page.evaluate(() => scrollY)).toBe(previousScroll);

      await more.tap();
      await menu.getByRole('link', { name: 'About', exact: true }).tap();
      await expect(page).toHaveURL(/\/about$/);
      await expect(page.locator('.about-hero')).toBeVisible();
      await expect(page.locator('#main-content')).toBeFocused();
      // Normal motion used to leave the shorter About page at its bottom;
      // reduced motion also let focus scroll past the top brand bar.
      await expect.poll(() => page.evaluate(() => scrollY)).toBeLessThanOrEqual(1);
      await expect(menu).not.toBeVisible();
      expect(await page.evaluate(() => scrollY)).toBeLessThanOrEqual(1);
      // Flush resize notifications from the removed Weather map before
      // checking errors, then confirm a return visit gets a working map.
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      expect(errors).toEqual([]);
      await page.goBack();
      await expect(page.locator('#radar-map')).toHaveClass(/leaflet-container/);
      expect(errors).toEqual([]);
    });
  }
});

async function ready(page: Page, path: string) {
  await page.goto(path);
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  await page.evaluate(() => document.fonts.ready);
}
async function appearance(page: Page, style: string, theme = 'light') {
  await page.addInitScript(({ style, theme }) => {
    if (!localStorage.getItem('style')) localStorage.setItem('style', style);
    if (!localStorage.getItem('theme')) localStorage.setItem('theme', theme);
  }, { style, theme });
}
async function info(page: Page, configured: boolean, demo = false) {
  await page.route(/\/api\/(v1\/)?info$/, async route => {
    const response = await route.fetch();
    await route.fulfill({ json: { ...await response.json(), demo, location_configured: configured } });
  });
}
async function readable(page: Page, include = '#main-content') {
  // Hydration applies the stored palette. Measure its settled colors, not
  // an intermediate frame of the dark-to-light surface transition.
  await page.evaluate(async () => {
    await Promise.all(document.getAnimations()
      .filter(a => a.effect?.getComputedTiming().endTime !== Infinity)
      .map(a => a.finished.catch(() => {})));
  });
  expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), 'horizontal overflow').toBe(false);
  const axe = await new AxeBuilder({ page }).include(include).withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
  expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
}

for (const width of [390, 1440]) {
  test(`rain decision history stays out of the current irrigation status at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await appearance(page, 'field');
    const line = 'Recent rain prompted skip decisions on 3 days in the last 30.';
    await page.route(/\/api\/(v1\/)?irrigation\/tuning(?:\?.*)?$/, route => route.fulfill({ json: {
      generated_epoch: 1, window_days: 30, zones: [],
      scorecard: { window_days: 30, scored_days: null, confirmed_days: null, min_scored_days: 3,
        line: '', reactive_days: 3, reactive_line: line },
    } }));
    await ready(page, '/irrigation');
    await expect(page.locator('.tuning-strip')).toHaveCount(0);
    await expect(page.getByText(line, { exact: true })).toHaveCount(0);
    await ready(page, '/history');
    await page.locator('.hist-forecast-review > summary').click();
    const summary = page.getByRole('region', { name: 'Rain skip decisions', exact: true });
    await expect(summary).toContainText(line);
    await expect(summary).toContainText('rule recommendations, not completed runs');
    await readable(page, '.hist-forecast-review');
  });
}

for (const [theme, choice] of [['light', 'classic'], ['dark', 'field'], ['auto', 'classic'], ['hc', 'field']]) {
  test(`upgrade preserves ${theme} while choosing ${choice}`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 900 });
    await page.addInitScript(t => { if (!localStorage.getItem('theme')) localStorage.setItem('theme', t); }, theme);
    await info(page, true);
    const writes: string[] = [];
    page.on('request', r => { if (r.url().includes('/api/') && r.method() !== 'GET') writes.push(r.url()); });
    await ready(page, '/history');
    await expect(page.getByRole('region', { name: 'Make yourself at home' })).toBeVisible();
    await expect(page.locator('html')).toHaveAttribute('data-style', 'classic');
    await readable(page, '.theme-upgrade');
    await page.getByRole('button', { name: choice === 'classic' ? /Keep Classic Blue/ : /Use Field Green/ }).click();
    await expect(page.locator('.theme-upgrade')).toHaveCount(0);
    expect(await page.evaluate(() => localStorage.getItem('style'))).toBe(choice);
    expect(await page.evaluate(() => localStorage.getItem('theme'))).toBe(theme);
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
    await expect(page.locator('html')).toHaveAttribute('data-style', choice);
    await expect(page.locator('.theme-upgrade')).toHaveCount(0);
    expect(writes).toEqual([]);
  });
}

test('upgrade dismissal defers the choice; explicit preferences and fresh/demo installs stay respected', async ({ page }) => {
  await info(page, true);
  await ready(page, '/history');
  await page.getByRole('button', { name: 'Decide later' }).click();
  expect(await page.evaluate(() => localStorage.getItem('style'))).toBeNull();
  await expect(page.locator('html')).toHaveAttribute('data-style', 'classic');
  await page.reload();
  await expect(page.locator('.theme-upgrade')).toBeVisible();
  // A previously selected style is an explicit choice, even if made in 0.9.4.
  for (const style of ['slate', 'field', 'classic']) {
    await page.evaluate(s => localStorage.setItem('style', s), style);
    await ready(page, '/history');
    await expect(page.locator('html')).toHaveAttribute('data-style', style);
    await expect(page.locator('.theme-upgrade')).toHaveCount(0);
  }
  await page.evaluate(() => localStorage.removeItem('style'));
  await info(page, false);
  await ready(page, '/settings/theme');
  await expect.poll(() => page.evaluate(() => localStorage.getItem('style'))).toBe('field');
  await expect(page.locator('.theme-upgrade')).toHaveCount(0);
  await page.evaluate(() => localStorage.removeItem('style'));
  await info(page, true, true);
  await ready(page, '/history');
  await expect(page.locator('html')).toHaveAttribute('data-style', 'field');
  await expect(page.locator('.theme-upgrade')).toHaveCount(0);
});

test('blocked browser storage does not break the upgrade choice', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, 'localStorage', { get() { throw new DOMException('Blocked', 'SecurityError'); } });
  });
  const errors: string[] = []; page.on('pageerror', e => errors.push(e.message));
  await info(page, true);
  await ready(page, '/history');
  await page.getByRole('button', { name: /Use Field Green/ }).click();
  await expect(page.locator('html')).toHaveAttribute('data-style', 'field');
  await expect(page.locator('.theme-upgrade')).toHaveCount(0);
  expect(errors).toEqual([]);
});

const compare = (metric: string, value: number) => ({ compare: { metric, op: 'gt', value } });
function configFixture() {
  return {
    untouched: 'preserve', engine: { skip_rules: { disabled_rules: [] as string[] } },
    conditions: { rules: [
      { id: 'wind', name: 'Wind guard', enabled: true, scope: 'all_zones', condition: compare('wind_now_mph', 12), action: 'skip' },
      { id: 'nested', name: 'Nested rule', enabled: false, scope: 'all_zones', condition: { not: compare('zone_soil_pct', 70) }, action: 'skip' },
    ] as any[] },
  };
}

test('rule loading failures retry; failed/slow saves preserve state and prevent duplicate submissions', async ({ page }) => {
  await appearance(page, 'field');
  let config = configFixture(), failLoad = true, failSave = true;
  let release: (() => void) | undefined;
  const writes: any[] = [];
  await page.route(/\/api\/(v1\/)?config$/, async route => {
    if (route.request().method() === 'PUT') {
      writes.push(route.request().postDataJSON());
      await new Promise<void>(resolve => { release = resolve; });
      if (failSave) return route.fulfill({ status: 503, json: { error: 'Save unavailable' } });
      config = writes.at(-1);
      return route.fulfill({ json: { ok: true } });
    }
    return route.fulfill(failLoad ? { status: 503, json: { error: 'Unavailable' } } : { json: config });
  });
  await ready(page, '/rules');
  await expect(page.getByRole('button', { name: '+ New rule' })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Reload rules', exact: true })).toBeVisible();
  failLoad = false;
  await page.getByRole('button', { name: 'Reload rules', exact: true }).click();
  await page.getByRole('button', { name: 'Reload custom rules', exact: true }).click();
  const wind = page.getByRole('switch', { name: 'Wind too high now rule', exact: true });
  await expect(wind).toHaveAttribute('aria-checked', 'true');
  await wind.click();
  await page.getByRole('button', { name: 'Turn off rule', exact: true }).click();
  await expect.poll(() => writes.length).toBe(1);
  await expect(wind).toBeDisabled();
  await expect(wind).toHaveAttribute('aria-checked', 'true');
  release!();
  await expect(page.getByRole('alert').filter({ hasText: 'Could not confirm the rule change' })).toBeVisible();
  await expect(wind).toBeEnabled();
  await expect(wind).toHaveAttribute('aria-checked', 'true');
  // Current fields are fetched again before saving, not overwritten by stale UI.
  config.untouched = 'changed elsewhere'; failSave = false;
  await wind.click();
  await page.getByRole('button', { name: 'Turn off rule', exact: true }).click();
  await expect.poll(() => writes.length).toBe(2); release!();
  await expect(wind).toHaveAttribute('aria-checked', 'false');
  expect(config.untouched).toBe('changed elsewhere');
  expect(config.conditions.rules).toHaveLength(2);
  await wind.click(); await expect.poll(() => writes.length).toBe(3); release!();
  await expect(wind).toHaveAttribute('aria-checked', 'true');
});

test('custom editor keeps template operands, unknown evidence, failed edits and concurrent changes', async ({ page }) => {
  test.setTimeout(60_000);
  await appearance(page, 'slate');
  let config = configFixture(), failSave = true;
  const writes: any[] = [];
  await page.route(/\/api\/(v1\/)?config$/, async route => {
    if (route.request().method() === 'PUT') {
      writes.push(route.request().postDataJSON());
      if (failSave) return route.fulfill({ status: 503, json: { error: 'Save unavailable' } });
      config = writes.at(-1); return route.fulfill({ json: { ok: true } });
    }
    return route.fulfill({ json: config });
  });
  await ready(page, '/rules');
  const nested = page.locator('.cond-row').filter({ hasText: 'Nested rule' });
  await expect(nested.getByRole('button', { name: 'Edit', exact: true })).toBeDisabled();
  await expect(nested).toContainText('configuration file');
  await page.locator('.cond-row').filter({ hasText: 'Wind guard' }).getByRole('button', { name: 'Edit', exact: true }).click();
  const editor = page.locator('.cond-editor');
  await expect(editor.getByLabel('Measurement', { exact: true })).toHaveValue('wind_now_mph');
  await expect(editor.getByLabel('Threshold', { exact: true })).toHaveValue('12');
  await editor.getByLabel('Name', { exact: true }).fill('Edited wind guard');
  await editor.getByLabel('Threshold', { exact: true }).fill('-1'); // Known true wind test.
  await editor.getByRole('button', { name: '+ Add condition' }).click();
  await editor.getByLabel('Measurement', { exact: true }).nth(1).selectOption('zone_soil_pct');
  await expect(editor.locator('.cond-editor__preview')).toContainText('Needs a zone reading');
  await editor.getByLabel('Match conditions', { exact: true }).selectOption('any');
  await expect(editor.locator('.cond-editor__preview')).toHaveText('Would fire now');
  await editor.getByLabel('Match conditions', { exact: true }).selectOption('all');
  await editor.getByLabel('Threshold', { exact: true }).first().fill('100000');
  await expect(editor.locator('.cond-editor__preview')).toHaveText('Would not fire now');
  await editor.getByRole('button', { name: 'Save rule' }).click();
  await expect(editor.getByRole('alert')).toContainText('not confirmed saved');
  await expect(editor.getByLabel('Name', { exact: true })).toHaveValue('Edited wind guard');
  expect(config.conditions.rules[0].name).toBe('Wind guard');
  await readable(page, '.cond-editor');
  // The editor must not overwrite a changed list with its old index.
  config.conditions.rules.reverse(); failSave = false;
  await editor.getByRole('button', { name: 'Save rule' }).click();
  await expect(editor.getByRole('alert')).toContainText('Rules changed elsewhere');
  expect(writes).toHaveLength(1);
  await editor.getByRole('button', { name: 'Cancel' }).click();
  await ready(page, '/rules');
  await page.locator('.cond-row').filter({ hasText: 'Wind guard' }).getByRole('button', { name: 'Edit', exact: true }).click();
  await editor.getByLabel('Name', { exact: true }).fill('Saved wind guard');
  await editor.getByRole('button', { name: 'Save rule' }).click();
  await expect(editor).toHaveCount(0);
  expect(config.conditions.rules.find(r => r.id === 'wind').name).toBe('Saved wind guard');
  expect(config.conditions.rules.find(r => r.id === 'nested').condition).toEqual({ not: compare('zone_soil_pct', 70) });
  await page.getByText('Example rules', { exact: true }).click();
  await page.locator('.rule-template').filter({ hasText: 'Skip cold mornings' }).getByRole('button', { name: 'Add', exact: true }).click();
  const template = page.locator('.cond-row').filter({ hasText: 'Skip cold mornings' });
  await template.getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(editor.getByLabel('Measurement', { exact: true })).toHaveValue('temp_now_f');
  await expect(editor.getByLabel('Threshold', { exact: true })).toHaveValue('45');
});

const trace = {
  verdict: 'skip', reason: 'Wind is above the limit', degraded: false,
  rules: [
    { id: 'freeze_now', label: 'Freeze risk now', category: 'weather', detail: 'Above freezing', outcome: 'passed', verdict: null },
    { id: 'rain_now', label: 'Currently raining', category: 'weather', detail: 'disabled by operator', outcome: 'skipped', verdict: null },
    { id: 'soil_frost', label: 'Soil frost', category: 'soil', detail: 'No soil thermometer', outcome: 'skipped', verdict: null },
    { id: 'wind_now', label: 'Wind too high now', category: 'weather', detail: 'Over the wind limit', outcome: 'fired', verdict: 'skip' },
    { id: 'heat', label: 'Heat boost', category: 'heat', detail: 'An earlier rule decided', outcome: 'not_reached', verdict: null },
    { id: 'rain', label: 'Rain prediction', category: 'weather', detail: 'Rain predicted', outcome: 'fired', verdict: 'skip', overridden_by: 'soil_model', overridden_detail: 'The soil balance already accounts for this rain' },
  ],
};
async function decisions(page: Page) {
  await page.route(/\/api\/(v1\/)?irrigation\/decisions\?days=30$/, route => route.fulfill({ json: {
    from_epoch: 1788000000, to_epoch: 1790000000,
    decisions: [{ epoch: 1789000000, verdict: 'skip', reason: trace.reason, trace }],
  } }));
}

test('decision path preserves unknown evidence, historical wording and the actual deciding rule', async ({ page }) => {
  await appearance(page, 'field');
  await page.setViewportSize({ width: 320, height: 900 });
  let selected: any = {
    verdict: 'skip', reason: 'Rain forecast unavailable; watering held', degraded: true,
    rules: [{ id: 'planning_forecast', label: 'Watering-plan rain availability', category: 'safety',
      detail: 'Rain forecast unavailable; watering held', outcome: 'fired', verdict: 'skip' }],
  };
  await page.route(/\/api\/(v1\/)?irrigation\/decisions\?days=30$/, route => route.fulfill({ json: {
    from_epoch: 0, to_epoch: 1790000000,
    decisions: [{ epoch: 1789000000, verdict: selected.verdict, reason: selected.reason, trace: selected }],
  } }));
  async function openRecord() {
    await ready(page, '/rules?tab=decisions');
    await page.locator('.rulelab-history__item').nth(1).click();
  }
  await openRecord();
  await expect(page.locator('.rulelab-verdict__reason')).toHaveText('Rain forecast unavailable; watering skipped');
  await expect(page.locator('.rule-row__detail')).toHaveText('Rain forecast unavailable; watering skipped');
  await expect(page.locator('.rulelab-unchecked')).toHaveCount(0);
  await readable(page);
  selected = { verdict: 'run', reason: 'Soil needs water', degraded: false, rules: [
    { id: 'rain', label: 'Rain prediction', category: 'weather', detail: 'Rain expected', outcome: 'fired', verdict: 'skip',
      overridden_by: 'soil_model', overridden_detail: 'Soil balance already accounts for this rain' },
    { id: 'soil_floor', label: 'Soil water balance', category: 'soil', detail: 'Soil needs water', outcome: 'fired', verdict: 'run' },
  ] };
  await openRecord();
  await expect(page.locator('.rulelab-verdict__pill')).toHaveText('Watering allowed');
  await expect(page.locator('.rulelab-verdict__source')).toHaveText('Check 2 · Soil water balance');
  await expect(page.locator('.rule-row__badge')).toHaveText(['Overridden', 'Deciding rule']);
  selected = { verdict: 'unknown', reason: '', degraded: false, rules: [] };
  await openRecord();
  await expect(page.locator('.rulelab-verdict__pill')).toHaveText('Decision unavailable');
  await expect(page.locator('.rulelab-verdict__reason')).toHaveText('No reason was recorded.');
});

for (const style of ['field', 'slate', 'classic']) for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
  test(`${style} ${theme} at ${width}px: History, rules, settings and feedback stay readable`, async ({ page }) => {
    test.setTimeout(120_000);
    await appearance(page, style, theme);
    await page.setViewportSize({ width, height: 900 });
    const errors: string[] = []; page.on('pageerror', e => errors.push(e.message));
    await decisions(page);
    await page.route(/\/api\/(v1\/)?config$/, route => route.fulfill({ json: configFixture() }));
    for (const path of ['/history', '/rules?tab=decisions', '/rules', '/settings/theme']) {
      await ready(page, path);
      if (path === '/history') {
        const daily = page.getByRole('button', { name: 'Daily log', exact: true });
        const runs = page.getByRole('button', { name: 'Run log', exact: true });
        expect(await daily.evaluate(e => getComputedStyle(e).borderTopColor)).not.toBe(await runs.evaluate(e => getComputedStyle(e).borderTopColor));
        await daily.click(); await expect(daily).toContainText('Viewing daily log');
        await runs.focus(); await page.keyboard.press('Enter');
        await expect(runs).toContainText('Viewing run log');
        await page.goBack(); await expect(daily).toHaveAttribute('aria-pressed', 'true');
      }
      if (path.includes('tab=decisions')) {
        await page.locator('.rulelab-history__item').nth(1).click();
        await expect(page.locator('.rulelab-verdict__eyebrow')).toContainText('Recorded decision');
        await expect(page.locator('.rulelab-verdict__pill')).toHaveText('Watering skipped');
        await expect(page.locator('.rulelab-verdict__source')).toHaveText('Check 4 · Wind too high now');
        await expect(page.locator('.rule-row__badge:visible')).toHaveText(['Passed', 'Off', 'Not applicable', 'Deciding rule', 'Overridden']);
        await expect(page.locator('.rule-row.is-muted')).not.toBeVisible();
        const unchecked = page.locator('.rulelab-unchecked summary');
        await expect(unchecked).toHaveText('Not evaluated · 1 check');
        await unchecked.focus(); await page.keyboard.press('Enter');
        await expect(page.locator('.rule-row.is-muted')).toBeVisible();
        await expect(page.locator('.rule-row.is-muted')).toHaveAttribute('value', '5');
        await expect(page.locator('.rule-row.is-overridden')).toHaveAttribute('value', '6');
        expect(await page.locator('.rule-row.is-muted').evaluate(e => getComputedStyle(e).opacity)).toBe('1');
      }
      if (path === '/rules') await expect(page.locator('.gate-row')).not.toHaveCount(0);
      await readable(page);
      await page.screenshot({ path: test.info().outputPath(`${style}-${theme}-${width}-${path.replace(/\W+/g, '-')}.png`), fullPage: true });
    }
    const feedback = page.getByRole('button', { name: 'Feedback', exact: true });
    await feedback.click();
    const dialog = page.getByRole('dialog', { name: 'Send feedback', exact: true });
    await expect(dialog).toBeVisible();
    await dialog.getByLabel('Your feedback').fill('Keyboard review; do not send.');
    await readable(page, '#beta-fb-sheet');
    await page.keyboard.press('Escape');
    await expect(dialog).not.toBeVisible();
    await expect(feedback).toBeFocused();
    expect(errors).toEqual([]);
  });
}

test('recorded-decision failures are distinct from empty history and can retry', async ({ page }) => {
  await appearance(page, 'field');
  let failed = true;
  await page.route(/\/api\/(v1\/)?irrigation\/decisions\?days=30$/, route => route.fulfill(failed
    ? { status: 503, json: { error: 'Unavailable' } }
    : { json: { from_epoch: 0, to_epoch: 1, decisions: [] } }));
  await ready(page, '/rules?tab=decisions');
  await expect(page.locator('.rulelab-history').getByRole('alert')).toContainText('Recorded decisions could not be loaded');
  failed = false;
  await page.getByRole('button', { name: 'Retry decisions' }).click();
  await expect(page.getByText('No recorded decisions in the last 30 days.')).toBeVisible();
  await expect(page.locator('.rulelab-history').getByRole('alert')).toHaveCount(0);
});

test('Classic Blue follows system appearance and retains the high-contrast palette', async ({ page }) => {
  test.setTimeout(60_000);
  await appearance(page, 'classic', 'auto');
  await page.setViewportSize({ width: 320, height: 900 });
  await page.emulateMedia({ colorScheme: 'light' });
  await ready(page, '/settings/theme');
  await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute('content', '#f6f9ff');
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute('content', '#0b1220');
  await page.getByRole('combobox', { name: 'Display mode', exact: true }).selectOption('hc');
  await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute('content', /^#000(?:000)?$/);
  await readable(page);
  await ready(page, '/history');
  await readable(page);
  await expect(page.locator('html')).toHaveAttribute('data-style', 'classic');
  await expect(page.getByRole('combobox', { name: 'Display mode', exact: true })).toHaveValue('hc');
});

for (const style of ['field', 'slate', 'classic']) for (const theme of ['light', 'dark']) {
  test(`${style} ${theme}: populated devices, zone badges, simulator and empty rules are accessible`, async ({ page }) => {
    test.setTimeout(90_000);
    await appearance(page, style, theme);
    await page.setViewportSize({ width: theme === 'light' ? 390 : 1440, height: 900 });
    const snapshot = await (await page.request.get('/api/v1/irrigation/snapshot')).json();
    await ready(page, `/zones/${snapshot.zones[0].slug}`);
    await expect(page.locator('.zone-detail__status')).toBeVisible();
    await readable(page);
    await ready(page, '/sensors');
    await expect(page.locator('.sensor-row.is-active')).toBeVisible();
    await readable(page);
    await ready(page, '/settings?section=devices');
    await expect(page.locator('.data-source-chain__nature').first()).toBeVisible();
    await readable(page);
    await ready(page, '/simulator');
    await expect(page.locator('.sim-verdict')).toBeVisible();
    await expect(page.getByRole('spinbutton', { name: 'Humidity', exact: true })).toBeVisible();
    await readable(page);
    await page.route(/\/api\/(v1\/)?config$/, route => route.fulfill({ json: { conditions: { rules: [] } } }));
    await ready(page, '/rules');
    await expect(page.getByText('No custom rules yet. Add a rule or start with an example below.')).toBeVisible();
    await readable(page);
  });
}

test('simulator rejects stale responses and provides retry after failure', async ({ page }) => {
  await appearance(page, 'field');
  let releaseOld: (() => void) | undefined, fail = false;
  let oldFinished: (() => void) | undefined;
  const finished = new Promise<void>(resolve => { oldFinished = resolve; });
  await page.route(/\/api\/(v1\/)?irrigation\/simulate$/, async route => {
    const wind = route.request().postDataJSON().wind_now_mph;
    if (wind === 12) await new Promise<void>(resolve => { releaseOld = resolve; });
    if (fail) return route.fulfill({ status: 503, json: { error: 'Scenario unavailable' } });
    const baseline = { verdict: 'run', reason: 'Baseline', rules: [] };
    await route.fulfill({ json: { baseline, hypothetical: { ...baseline, verdict: 'skip', reason: `Scenario ${wind}` } } });
    if (wind === 12) oldFinished!();
  });
  await ready(page, '/simulator');
  await expect(page.locator('.sim-verdict')).toBeVisible();
  const wind = page.getByRole('spinbutton', { name: 'Wind now (mph)', exact: true });
  await wind.fill('12');
  await expect.poll(() => Boolean(releaseOld)).toBe(true);
  await expect(page.locator('.sim-result')).toHaveAttribute('aria-busy', 'true');
  await expect(page.locator('.sim-verdict')).toHaveCount(0);
  await wind.fill('13');
  await expect(page.locator('.sim-verdict__reason')).toHaveText('Scenario 13');
  releaseOld!(); await finished;
  await page.waitForTimeout(150); // Allow the deliberately late response to reach WASM.
  await expect(page.locator('.sim-verdict__reason')).toHaveText('Scenario 13');
  fail = true; await wind.fill('14');
  await expect(page.locator('.sim-result').getByRole('alert')).toContainText('Simulation unavailable');
  await expect(page.locator('.sim-verdict')).toHaveCount(0);
  fail = false;
  await page.getByRole('button', { name: 'Retry simulation', exact: true }).click();
  await expect(page.locator('.sim-verdict__reason')).toHaveText('Scenario 14');
  await expect(page.locator('.sim-result').getByRole('alert')).toHaveCount(0);
  await expect(wind).toHaveValue('14');
});
