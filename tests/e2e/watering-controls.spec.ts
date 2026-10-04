import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { runInNewContext } from 'node:vm';
import demo from './fixtures/visual-demo.json';

async function fixture(page: Page, style = 'field', theme = 'dark', quick = true) {
  await page.addInitScript(({ style, theme }) => {
    localStorage.setItem('style', style); localStorage.setItem('theme', theme);
  }, { style, theme });
  const responses: any = structuredClone(demo.responses);
  const zones = responses['/api/irrigation/quick-run'].zones;
  let run: any = quick ? { id: 'watering-session', request_id: 'start-once', phase: 'running',
    zones: zones.map((z: any) => ({ zone: z.zone, seconds: 300 })), names: zones.map((z: any) => z.name),
    completed: 0, current: 0, current_ends_epoch: Math.floor(Date.now() / 1000) + 300,
    started_epoch: Math.floor(Date.now() / 1000), message: 'Watering', stop_unconfirmed: false } : null;
  let fail = false;
  let partial = false;
  let device = false;
  let release: (() => void) | undefined;
  let delay = false;
  const writes: any[] = [];
  const streams: any = { '/api/stream': '/api/snapshot', '/api/forecast/stream': '/api/forecast/snapshot', '/api/irrigation/stream': '/api/irrigation/snapshot' };
  await page.route('**/api/**', async route => {
    const req = route.request(), url = new URL(req.url());
    const key = url.pathname.replace('/api/v1/', '/api/') + url.search;
    if (key === '/api/irrigation/quick-run' && req.method() === 'GET') return route.fulfill({ json: { available: true, zones, run, reason: null } });
    if (req.method() !== 'GET') {
      const body = req.postDataJSON(); writes.push({ key, body });
      if (delay) await new Promise<void>(resolve => { release = resolve; });
      if (fail) return route.fulfill({ status: 502, json: { error: 'Stop was not confirmed. Retry Stop.' } });
      if (key === '/api/irrigation/quick-run/stop') {
        run = { ...run, phase: 'stopped', stop_unconfirmed: false, message: 'Remaining zones will not start.' };
        return route.fulfill({ json: run });
      }
      if (key === '/api/irrigation/quick-run') {
        run = { id: 'watering-session', request_id: body.request_id, phase: 'running', zones: body.zones,
          names: body.zones.map((z: any) => zones.find((choice: any) => choice.zone === z.zone).name),
          completed: 0, current: 0, current_ends_epoch: Math.floor(Date.now() / 1000) + 300,
          started_epoch: Math.floor(Date.now() / 1000), message: 'Watering', stop_unconfirmed: false };
        return route.fulfill({ json: run });
      }
      return route.fulfill({ json: { ok: true, scope: device ? 'device' : 'zone', failed: partial ? ['controller'] : [] } });
    }
    const value = responses[streams[key] ?? key];
    if (value === undefined) return route.continue();
    return streams[key] ? route.fulfill({ contentType: 'text/event-stream', body: `event: snapshot\ndata: ${JSON.stringify(value)}\n\n` }) : route.fulfill({ json: value });
  });
  return { writes, fail: (value: boolean) => fail = value, partial: () => partial = true, delay: () => delay = true,
    meteredDevice: () => { device = true; responses['/api/irrigation/snapshot'].flow.rate_gpm = 1.8; },
    release: () => { delay = false; release?.(); },
    unconfirmed: () => run = { ...run, phase: 'failed', stop_unconfirmed: true },
    complete: () => run = { ...run, phase: 'stopped', stop_unconfirmed: false },
    running: (count = 1) => responses['/api/irrigation/snapshot'].zones.forEach((z: any, i: number) => { z.running = i < count; z.running_known = true; }) };
}

async function open(page: Page, path: string) {
  await page.goto(path);
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
}

for (const style of ['field', 'slate', 'classic']) for (const theme of ['light', 'dark']) for (const width of [320, 1440]) {
  test(`watering strip remains readable and reachable: ${style} ${theme} ${width}`, async ({ page }, info) => {
    const errors: string[] = []; page.on('pageerror', e => errors.push(e.message));
    await page.setViewportSize({ width, height: 900 });
    await fixture(page, style, theme);
    await open(page, style === 'field' ? '/' : style === 'slate' ? '/history' : '/settings');
    const strip = page.getByRole('region', { name: 'Current watering' });
    await expect(strip).toContainText('Back Yard');
    await expect(strip).toContainText('0 of 4 zones finished');
    const stop = strip.getByRole('button', { name: 'Stop Quick Run' });
    expect((await stop.boundingBox())!.height).toBeGreaterThanOrEqual(44);
    const view = await strip.getByRole('link', { name: 'View', exact: true }).boundingBox();
    expect(view!.width).toBeGreaterThanOrEqual(44);
    expect(view!.height).toBeGreaterThanOrEqual(44);
    await page.evaluate(() => scrollTo(0, 700));
    await expect(stop).toBeInViewport();
    expect(await strip.locator('strong').evaluate(el => {
      const r = el.getBoundingClientRect();
      return el.contains(document.elementFromPoint(r.left + 4, r.top + 1));
    }), 'the zone name must not sit under a fixed mode banner or header').toBe(true);
    expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
    expect(await strip.evaluate(el => el.scrollWidth > el.clientWidth)).toBe(false);
    const axe = await new AxeBuilder({ page }).include('.watering-strip').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
    expect(axe.violations.filter(v => ['serious', 'critical'].includes(v.impact ?? ''))).toEqual([]);
    await info.attach('watering-strip', { body: await page.screenshot(), contentType: 'image/png' });
    expect(errors).toEqual([]);
  });
}

test('a pending start survives SPA navigation; Stop retries and cancels the queue', async ({ page }) => {
  const state = await fixture(page, 'field', 'dark', false);
  await open(page, '/irrigation');
  await page.locator('.quick-run-entry button').click();
  const dialog = page.getByRole('dialog', { name: 'Quick Run', exact: true });
  await dialog.getByRole('button', { name: 'Select all', exact: true }).click();
  state.delay();
  await dialog.getByRole('button', { name: 'Start Quick Run', exact: true }).click();
  await expect.poll(() => state.writes.length).toBe(1);
  await page.keyboard.press('Escape');
  // A real Router-intercepted anchor, preserving the App owner during the request.
  await page.evaluate(() => { const a = document.createElement('a'); a.href = '/about'; document.body.append(a); a.click(); a.remove(); });
  await expect(page).toHaveURL(/\/about$/);
  state.release();
  const strip = page.getByRole('region', { name: 'Current watering' });
  await expect(strip).toContainText('Back Yard');
  state.fail(true);
  await strip.getByRole('button', { name: 'Stop Quick Run' }).click();
  await expect(strip).toContainText('Stop was not confirmed');
  await expect(strip.getByRole('button', { name: 'Stop Quick Run' })).toBeEnabled();
  state.fail(false);
  await strip.getByRole('button', { name: 'Stop Quick Run' }).click();
  await expect(strip).toHaveCount(0);
  expect(state.writes.slice(1).map(w => w.body)).toEqual([{ id: 'watering-session' }, { id: 'watering-session' }]);
  await expect(page.locator('.beta-fb__pill')).toContainText('Feedback');
});

test('unconfirmed Quick Run stays actionable in high contrast', async ({ page }) => {
  const state = await fixture(page, 'classic', 'hc'); state.unconfirmed();
  await page.setViewportSize({ width: 320, height: 900 }); await open(page, '/about');
  const strip = page.getByRole('region', { name: 'Current watering' });
  await expect(strip).toContainText('Check watering');
  await expect(strip).toContainText('Stop wasn’t confirmed');
  await expect(strip.getByRole('button', { name: 'Stop Quick Run' })).toBeEnabled();
});

for (const count of [1, 2]) test(`controller watering exposes ${count === 1 ? 'zone stop' : 'stop all'} across the app`, async ({ page }) => {
  const state = await fixture(page, 'field', 'light', false); state.running(count);
  await open(page, '/about');
  const strip = page.getByRole('region', { name: 'Current watering' });
  await expect(strip).toContainText('Watering now');
  await strip.getByRole('button', { name: count === 1 ? 'Stop' : 'Stop all', exact: true }).click();
  await expect(strip).toContainText('Stop sent');
  expect(state.writes[0].body.kind).toBe(count === 1 ? 'stop' : 'stop_all');
  if (count === 1) expect(state.writes[0].body.zone).toBe('back_yard');
  // An acknowledged command is not proof that the controller now reports idle.
  await expect(strip).toBeVisible();
});

test('an old Quick Run error cannot hide the result of stopping controller-reported watering', async ({ page }) => {
  const state = await fixture(page); state.running();
  await open(page, '/about');
  const strip = page.getByRole('region', { name: 'Current watering' });
  state.fail(true);
  await strip.getByRole('button', { name: 'Stop Quick Run' }).click();
  await expect(strip).toContainText('Stop was not confirmed');
  state.complete(); state.fail(false);
  await expect(strip).toContainText('Watering now');
  await strip.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(strip).toContainText('Stop sent');
  await expect(strip).not.toContainText('not confirmed');
});

test('the strip retains measured flow and explains device-wide Stop', async ({ page }) => {
  const state = await fixture(page, 'field', 'light', false); state.running(); state.meteredDevice();
  await open(page, '/about');
  const strip = page.getByRole('region', { name: 'Current watering' });
  await expect(strip).toContainText('Flow: 1.8 gpm');
  await strip.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(strip).toContainText('Stop sent for all zones on this controller');
});

test('partial Stop all acknowledgement stays visible with retry', async ({ page }) => {
  const state = await fixture(page, 'field', 'dark', false); state.running(2); state.partial();
  await open(page, '/about');
  const strip = page.getByRole('region', { name: 'Current watering' });
  await strip.getByRole('button', { name: 'Stop all', exact: true }).click();
  await expect(strip).toContainText('Stop wasn’t confirmed');
  await expect(strip.getByRole('button', { name: 'Stop all', exact: true })).toBeEnabled();
  await expect(strip).not.toContainText('Stop sent');
});

function worker(source: string, scope = 'https://sky.example/') {
  const listeners: any = {}, notifications: any[] = [], requests: any[] = [], opened: string[] = [];
  let response: any = { ok: true, status: 200, json: async () => ({ ok: true, scope: 'zone' }) };
  const self = { registration: { scope, showNotification: async (title: string, options: any) => { notifications.push({ title, ...options }); } },
    addEventListener: (name: string, fn: any) => listeners[name] = fn,
    clients: { matchAll: async () => [], openWindow: async (url: string) => { opened.push(url); } } };
  runInNewContext(source, { self, URL, AbortController, setTimeout, clearTimeout,
    fetch: async (url: string, options: any) => { requests.push({ url, ...options }); if (response instanceof Error) throw response; return response; } });
  return { notifications, requests, opened, response: (value: any) => response = value,
    emit: async (name: string, event: any) => { let done: any; listeners[name]({ ...event, waitUntil: (p: any) => done = p }); await done; } };
}
const stopData = { zone: 'back_yard', run_id: 'original-run' };
function click(action = 'stop-watering', data: any = { url: '/irrigation', stop: stopData }) {
  return { action, notification: { data, close: () => {} } };
}

test('installed worker exposes Stop and sends one authenticated, scoped command', async ({ request }) => {
  const source = await (await request.get('/sw.js')).text();
  const sw = worker(source, 'https://sky.example/ingress/');
  await sw.emit('push', { data: { json: () => ({ title: 'Back Yard started', stop: stopData }) } });
  expect(sw.notifications[0].actions.map((a: any) => a.action)).toEqual(['stop-watering', 'open']);
  expect(sw.notifications[0].icon).toBe('https://sky.example/ingress/icons/icon-192.png');
  await sw.emit('notificationclick', click());
  expect(sw.requests).toHaveLength(1);
  expect(sw.requests[0]).toMatchObject({ url: 'https://sky.example/ingress/api/v1/irrigation/notification-stop', method: 'POST', credentials: 'same-origin', redirect: 'error' });
  expect(JSON.parse(sw.requests[0].body)).toEqual(stopData);
  expect(sw.notifications.at(-1).title).toBe('Stop sent');
  expect(sw.opened).toEqual([]);
});

for (const failure of ['offline', 'unauthorized', 'stale', 'invalid-success']) test(`notification Stop handles ${failure} honestly and opens watering controls`, async ({ request }) => {
  const sw = worker(await (await request.get('/sw.js')).text());
  sw.response(failure === 'offline' ? new Error('offline') : {
    ok: failure === 'invalid-success', status: failure === 'stale' ? 409 : failure === 'unauthorized' ? 401 : 200,
    json: async () => ({ error: 'not confirmed' }),
  });
  await sw.emit('notificationclick', click());
  expect(sw.requests).toHaveLength(1);
  expect(sw.notifications.at(-1).title).toBe(failure === 'stale' ? 'Run already changed' : 'Stop wasn’t confirmed');
  expect(sw.opened).toEqual(['https://sky.example/irrigation']);
});

test('ordinary and legacy notification taps never water or stop; external links stay in the app', async ({ request }) => {
  const sw = worker(await (await request.get('/sw.js')).text());
  await sw.emit('notificationclick', click(''));
  await sw.emit('notificationclick', click('', '/zones/back_yard'));
  await sw.emit('notificationclick', click('open', { url: 'https://untrusted.example/' }));
  expect(sw.requests).toEqual([]);
  expect(sw.opened).toEqual(['https://sky.example/irrigation', 'https://sky.example/zones/back_yard', 'https://sky.example/irrigation']);
});
