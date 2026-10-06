import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.use({ serviceWorkers: 'block', reducedMotion: 'reduce', channel: 'chromium', launchOptions: {
  // The isolated CI fixture uses a Docker HTTP hostname. Expose browser push
  // APIs for mocks only; no real permission prompt or subscription is used.
  // Full Chromium honors this flag; the separate headless shell does not.
  args: [
    `--unsafely-treat-insecure-origin-as-secure=${new URL(process.env.BASE_URL ?? 'http://localhost:8091').origin}`,
    ...(process.env.CI ? ['--no-sandbox', '--disable-dev-shm-usage'] : []),
  ],
} });

async function device(page: Page, style = 'field', theme = 'dark') {
  await page.addInitScript(({ style, theme }) => {
    localStorage.setItem('style', style); localStorage.setItem('theme', theme);
    Object.defineProperty(Notification, 'permission', { get: () => 'granted' });
    const sub = Object.create(PushSubscription.prototype);
    Object.defineProperties(sub, {
      endpoint: { value: 'https://push.example/this-device' },
      getKey: { value: () => new Uint8Array([1, 2, 3]).buffer },
    });
    const reg = Object.create(ServiceWorkerRegistration.prototype);
    Object.defineProperty(reg, 'pushManager', { value: { getSubscription: async () => sub } });
    Object.defineProperty(ServiceWorkerContainer.prototype, 'ready', { get: () => Promise.resolve(reg) });
  }, { style, theme });
  let saved: any = {
    enabled: true,
    events: ['watering_started', 'watering_finished', 'watering_problem', 'urgent_equipment', 'soil_sensor', 'weather_source', 'configuration'],
    quiet_hours: { enabled: true, start: '22:00', end: '07:00', allow_urgent: true },
    daily_outlook: { enabled: false, time: '09:00' },
  };
  let writes = 0, fail = false;
  await page.route(/\/api\/(v1\/)?push\/status$/, route => route.fulfill({ json: { ready: true, enabled: true, timezone: 'America/New_York' } }));
  await page.route(/\/api\/(v1\/)?push\/preferences(?:\/read)?$/, route => {
    const body = route.request().postDataJSON();
    expect(body.endpoint).toBe('https://push.example/this-device');
    expect(body.keys).toEqual({ p256dh: 'AQID', auth: 'AQID' });
    if (!route.request().url().endsWith('/read')) {
      writes++;
      if (fail) return route.fulfill({ status: 503, json: { error: 'Notification storage is unavailable.' } });
      saved = body.preferences;
    }
    return route.fulfill({ json: { preferences: saved } });
  });
  await page.route(/\/api\/(v1\/)?config$/, route => {
    expect(route.request().method()).toBe('GET');
    return route.fulfill({ json: { notifications: {} } });
  });
  return { saved: () => saved, writes: () => writes, fail: (value: boolean) => { fail = value; } };
}

async function open(page: Page) {
  await page.goto('/settings/notifications');
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  await expect(page.getByRole('switch', { name: /^Notifications on this device/ })).toHaveAttribute('aria-checked', 'true');
}

test('device settings retain choices, reject quiet-hour summaries and surface failed saves', async ({ page }) => {
  const fixture = await device(page);
  await open(page);
  const save = page.getByRole('button', { name: 'Save device settings', exact: true });
  await expect(save).toBeDisabled();
  await expect(page.getByLabel('Quiet hours start')).toHaveValue('22:00');
  await expect(page.getByRole('switch', { name: /^Urgent alerts during quiet hours/ })).toHaveAttribute('aria-checked', 'true');
  await page.getByRole('switch', { name: /^High wind/ }).click();
  await page.getByRole('switch', { name: /^Daily watering outlook on this device/ }).click();
  await page.getByLabel('Device outlook time').fill('00:00');
  await save.click();
  await expect(page.locator('.pwa-preferences__error')).toContainText('outside quiet hours');
  expect(fixture.writes()).toBe(0);
  await page.getByLabel('Device outlook time').fill('09:30');
  fixture.fail(true);
  await save.click();
  await expect(page.locator('.pwa-preferences__error')).toContainText('storage is unavailable');
  await expect(page.getByText('Unsaved changes', { exact: true })).toBeVisible();
  await expect(save).toBeEnabled();
  fixture.fail(false);
  await save.click();
  await expect(page.getByText('Saved for this device.', { exact: true })).toBeVisible();
  expect(fixture.saved().events).toContain('wind');
  expect(fixture.saved().daily_outlook).toEqual({ enabled: true, time: '09:30' });
  await page.reload();
  await expect(page.getByRole('switch', { name: /^High wind/ })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByLabel('Device outlook time')).toHaveValue('09:30');
  await expect(save).toBeDisabled();
});

for (const style of ['field', 'slate', 'classic']) for (const theme of ['light', 'dark']) {
  const width = theme === 'light' ? 1440 : (style === 'field' ? 320 : 390);
  test(`device choices are readable and accessible: ${style} ${theme} ${width}px`, async ({ page }) => {
    const errors: string[] = []; page.on('pageerror', e => errors.push(e.message));
    await page.setViewportSize({ width, height: 900 });
    await device(page, style, theme);
    await open(page);
    await page.evaluate(() => document.fonts.ready);
    await page.evaluate(async () => { await Promise.all(document.getAnimations().filter(a => a.effect?.getComputedTiming().endTime !== Infinity).map(a => a.finished.catch(() => {}))); });
    expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
    const axe = await new AxeBuilder({ page }).include('.pwa-preferences').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
    expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
    expect(errors).toEqual([]);
    await page.screenshot({ path: `test-results/notifications-${style}-${theme}-${width}.png`, fullPage: true });
  });
}

test('failed preference reads do not expose editable defaults', async ({ page }) => {
  await device(page);
  await page.route(/\/api\/(v1\/)?push\/preferences\/read$/, route => route.fulfill({ status: 503, json: { error: 'Notification storage is unavailable.' } }));
  await page.goto('/settings/notifications');
  await expect(page.locator('.pwa-preferences__error')).toContainText('storage is unavailable');
  await expect(page.getByRole('button', { name: 'Save device settings', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Check again', exact: true })).toBeEnabled();
});

test('device and shared switch labels remain distinct when both sections are open', async ({ page }) => {
  await device(page);
  await open(page);
  await page.getByText('Server and shared channels', { exact: true }).click();
  const shared = page.locator('.pwa-shared-channels');
  await expect(shared.getByRole('switch', { name: /^Send push alerts/ })).toHaveAttribute('aria-checked', 'true');
  await shared.getByText('Daily outlook for ntfy and Slack', { exact: true }).click();
  await expect(shared.getByRole('switch', { name: /^Daily outlook for ntfy and Slack/ })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByRole('switch', { name: /^Notifications on this device/ })).toHaveAttribute('aria-checked', 'true');
  await shared.getByText('Send push alerts', { exact: true }).click();
  await expect(shared.getByRole('switch', { name: /^Send push alerts/ })).toHaveAttribute('aria-checked', 'false');
  await expect(page.getByRole('switch', { name: /^Quiet hours/ })).toHaveAttribute('aria-checked', 'true');
  const ids = await page.getByRole('switch').evaluateAll(elements => elements.map(e => e.id));
  expect(new Set(ids).size).toBe(ids.length);
  const axe = await new AxeBuilder({ page }).include('.settings-page').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
  expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
});

test('an insecure browser explains the requirement without offering a broken connection action', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 900 });
  await device(page);
  await page.addInitScript(() => Object.defineProperty(window, 'isSecureContext', { value: false }));
  await page.goto('/settings/notifications');
  await expect(page.getByText('Use the installed PWA or an HTTPS browser', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Connect this device', exact: true })).toHaveCount(0);
  await expect(page.getByRole('switch', { name: /^Notifications on this device/ })).toHaveCount(0);
  await page.getByText('Server and shared channels', { exact: true }).click();
  expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
});
