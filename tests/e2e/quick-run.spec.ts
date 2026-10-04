import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const zones = [
  { zone: 'front', name: 'Front lawn', max_seconds: 3600 },
  { zone: 'back', name: 'Back yard', max_seconds: 1800 },
  { zone: 'side', name: 'Side yard and the long garden border', max_seconds: 1200 },
  { zone: 'beds', name: 'Flower beds', max_seconds: 900 },
];

async function fixture(page: Page, style = 'field', theme = 'light') {
  await page.addInitScript(({ style, theme }) => {
    localStorage.setItem('style', style);
    localStorage.setItem('theme', theme);
  }, { style, theme });
  let run: any = null;
  const starts: any[] = [];
  const stops: any[] = [];
  let failure = '';
  await page.route(/\/api\/(v1\/)?irrigation\/quick-run(?:\/stop)?$/, async route => {
    const request = route.request();
    if (request.method() === 'GET') return route.fulfill({ json: { available: true, zones, run, reason: null } });
    const body = request.postDataJSON();
    if (request.url().endsWith('/stop')) {
      stops.push(body);
      if (failure) return route.fulfill({ status: 503, json: { error: failure } });
      run = { ...run, phase: 'stopped', stop_unconfirmed: false, current_ends_epoch: null, message: 'Quick Run stopped. Remaining zones will not start.' };
    } else {
      starts.push(body);
      if (failure) return route.fulfill({ status: 409, json: { error: failure } });
      run = { id: 'fixture-session', request_id: body.request_id, phase: 'running', zones: body.zones,
        names: body.zones.map((choice: any) => zones.find(z => z.zone === choice.zone)!.name),
        completed: 0, current: 0, current_ends_epoch: Math.floor(Date.now() / 1000) + body.zones[0].seconds,
        started_epoch: Math.floor(Date.now() / 1000), message: 'Run command accepted. Actual watering appears in History when reported by the controller.' };
    }
    return route.fulfill({ json: run });
  });
  return { starts, stops, setFailure: (value: string) => failure = value,
    unconfirmedStop: () => run = { ...run, phase: 'failed', stop_unconfirmed: true, message: 'Stop was not confirmed. Check the controller or retry Stop all watering. Remaining zones were cancelled.' } };
}

async function open(page: Page, path = '/irrigation') {
  await page.goto(path);
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  await page.locator('.quick-run-entry').getByRole('button', { name: 'Quick Run', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Quick Run', exact: true });
  await expect(dialog).toBeVisible();
  return dialog;
}

for (const style of ['field', 'slate', 'classic']) {
  for (const theme of ['light', 'dark']) {
    for (const width of [320, 1440]) {
      test(`Quick Run picker is readable in ${style} ${theme} at ${width}px`, async ({ page }, info) => {
        const errors: string[] = [];
        page.on('pageerror', error => errors.push(error.message));
        await page.setViewportSize({ width, height: 900 });
        await fixture(page, style, theme);
        const dialog = await open(page, width === 320 ? '/irrigation' : '/zones');
        await expect(dialog.getByRole('button', { name: 'Start Quick Run', exact: true })).toBeDisabled();
        await dialog.getByRole('button', { name: 'Select all', exact: true }).click();
        expect((await dialog.getByRole('button', { name: 'Clear selection', exact: true }).boundingBox())!.height).toBeGreaterThanOrEqual(44);
        await expect(dialog).toContainText('20 min total');
        await expect(dialog).toContainText('4 zones selected');
        await expect(dialog.locator('.quick-run__time-error:visible')).toHaveCount(0);
        await expect(dialog.getByRole('button', { name: '5 min', exact: true })).toHaveAttribute('aria-pressed', 'true');
        expect(await dialog.locator('.quick-run__preset').evaluateAll(buttons => buttons.filter(button => {
          const number = [...button.childNodes].find(node => node.nodeType === Node.TEXT_NODE && /\d/.test(node.textContent ?? ''));
          if (!number) return true;
          const range = document.createRange(); range.selectNodeContents(number);
          const digits = range.getBoundingClientRect(), unit = button.querySelector('span')!.getBoundingClientRect();
          return Math.abs((digits.top + digits.bottom) / 2 - (unit.top + unit.bottom) / 2) > 4;
        }).map(button => button.textContent)), 'minutes must stay beside the number').toEqual([]);
        await page.evaluate(() => document.fonts.ready);
        await page.evaluate(async () => { await Promise.all(document.getAnimations().filter(a => a.effect?.getComputedTiming().endTime !== Infinity).map(a => a.finished.catch(() => {}))); });
        expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
        expect(await dialog.evaluate(el => el.scrollWidth > el.clientWidth)).toBe(false);
        const axe = await new AxeBuilder({ page }).include('#quick-run-dialog').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
        expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
        await info.attach('quick-run-picker', { body: await page.screenshot(), contentType: 'image/png' });
        await page.keyboard.press('Escape');
        await expect(dialog).not.toBeVisible();
        await expect(page.locator('.quick-run-entry button')).toBeFocused();
        expect(errors).toEqual([]);
      });
    }
  }
}

test('choose any zones, change individual times, keep progress after navigation and stop the queue', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const state = await fixture(page, 'field', 'dark');
  let dialog = await open(page);
  await dialog.getByRole('checkbox', { name: 'Back yard', exact: true }).check();
  await dialog.getByRole('checkbox', { name: 'Flower beds', exact: true }).check();
  await dialog.getByRole('spinbutton', { name: 'Minutes for Back yard', exact: true }).fill('8');
  await expect(dialog).toContainText('13 min total');
  await dialog.getByRole('button', { name: 'Start Quick Run', exact: true }).click();
  await expect(dialog).toContainText('Quick Run in progress');
  expect(state.starts).toHaveLength(1);
  expect(state.starts[0].zones).toEqual([{ zone: 'back', seconds: 480 }, { zone: 'beds', seconds: 300 }]);
  expect(state.starts[0].request_id).toBeTruthy();
  await page.keyboard.press('Escape');
  await page.goto('/zones');
  await page.locator('.quick-run-entry').getByRole('button', { name: 'View Quick Run', exact: true }).click();
  dialog = page.getByRole('dialog', { name: 'Quick Run', exact: true });
  await expect(dialog.locator('.quick-run__queue')).toContainText('Back yard');
  await expect(dialog.locator('.quick-run__queue')).toContainText('Flower beds');
  await dialog.getByRole('button', { name: 'Stop Quick Run', exact: true }).click();
  await expect(dialog).toContainText('Remaining zones will not start');
  expect(state.stops).toEqual([{ id: 'fixture-session' }]);
  expect(state.starts).toHaveLength(1);
});

test('preset, all-zone selection, limits, errors and retry use one request identity', async ({ page }) => {
  const state = await fixture(page);
  const dialog = await open(page);
  await dialog.getByRole('button', { name: '10 min', exact: true }).click();
  await dialog.getByRole('button', { name: 'Select all', exact: true }).click();
  await expect(dialog).toContainText('40 min total');
  await dialog.getByRole('button', { name: 'Clear selection', exact: true }).click();
  await expect(dialog).toContainText('0 zones selected');
  await dialog.getByRole('checkbox', { name: 'Flower beds', exact: true }).check();
  const duration = dialog.getByRole('spinbutton', { name: 'Minutes for Flower beds', exact: true });
  await duration.fill('16');
  await expect(dialog).toContainText('Choose 1–15 minutes for this zone.');
  await expect(dialog.getByRole('button', { name: 'Start Quick Run', exact: true })).toBeDisabled();
  await duration.fill('0');
  await expect(dialog.getByRole('button', { name: 'Start Quick Run', exact: true })).toBeDisabled();
  await duration.fill('3');
  state.setFailure('The controller is unavailable. Check its connection and try again.');
  await dialog.getByRole('button', { name: 'Start Quick Run', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('controller is unavailable');
  state.setFailure('');
  await dialog.getByRole('button', { name: 'Start Quick Run', exact: true }).click();
  await expect(dialog).toContainText('Quick Run in progress');
  expect(state.starts).toHaveLength(2);
  expect(state.starts[0].request_id).toBe(state.starts[1].request_id);
  state.setFailure('Stop could not be confirmed. Check the controller.');
  await dialog.getByRole('button', { name: 'Stop Quick Run', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('Stop could not be confirmed');
  await expect(dialog.getByRole('button', { name: 'Stop Quick Run', exact: true })).toBeEnabled();
});

test('high contrast keeps an unconfirmed stop visible and retryable', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 900 });
  const state = await fixture(page, 'classic', 'hc');
  const dialog = await open(page);
  await dialog.getByRole('checkbox', { name: 'Front lawn', exact: true }).check();
  await dialog.getByRole('button', { name: 'Start Quick Run', exact: true }).click();
  await expect(dialog).toContainText('Quick Run in progress');
  state.unconfirmedStop();
  await expect(dialog).toContainText('Quick Run needs attention');
  await page.keyboard.press('Escape');
  await expect(page.locator('.quick-run-entry')).toContainText('Stop wasn’t confirmed');
  await expect(page.locator('.quick-run-entry')).toContainText('Check Quick Run');
  await page.locator('.quick-run-entry').getByRole('button', { name: 'View Quick Run', exact: true }).click();
  await expect(dialog.getByRole('button', { name: 'New Quick Run', exact: true })).toHaveCount(0);
  const axe = await new AxeBuilder({ page }).include('#quick-run-dialog').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
  expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
  await dialog.getByRole('button', { name: 'Stop all watering', exact: true }).click();
  await expect(dialog).toContainText('Quick Run stopped');
  await expect(dialog.getByRole('button', { name: 'New Quick Run', exact: true })).toBeVisible();
});
