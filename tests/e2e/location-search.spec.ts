import { test, expect, Page } from '@playwright/test';

// The Location step's address box must never go quiet: hits list their
// source and fill the coordinates, an empty answer / rate limit / outage
// each say so, stale results clear when the query changes, and "Use my
// location" fills the coordinates from the browser or explains a refusal.
// The relay is mocked per mode so every state is reproducible without
// touching Nominatim or the Census geocoder.

const origin = new URL(process.env.BASE_URL ?? 'http://localhost:8091').origin;

test.use({ serviceWorkers: 'block', reducedMotion: 'reduce', channel: 'chromium', launchOptions: {
  // The isolated CI fixture reaches the app over a Docker HTTP hostname.
  // Geolocation is a secure-context API, so treat that origin as secure;
  // the button's own HTTPS message is covered by the server header test.
  args: [
    `--unsafely-treat-insecure-origin-as-secure=${origin}`,
    ...(process.env.CI ? ['--no-sandbox', '--disable-dev-shm-usage'] : []),
  ],
} });

type Mode = 'hits' | 'empty' | 'rate' | 'down';

// Demo-site coordinates (Orlando), never a real operator's.
const HITS = [
  { display_name: 'Orlando, Orange County, Florida, United States', lat: '28.5421', lon: '-81.3790', source: 'osm' },
  { display_name: '400 S Orange Ave, Orlando, FL 32801', lat: '28.538340', lon: '-81.379240', source: 'census' },
];

// Only this browser's draft changes; the relay, timezone and elevation
// lookups are mocked so the step is exercised, not the upstreams.
async function sandbox(page: Page) {
  let draft = await (await page.request.get('/api/wizard/draft')).json();
  draft.license_accepted = true;
  const geocode = { mode: 'hits' as Mode, queries: [] as string[] };
  await page.route('**/api/**', async route => {
    const url = new URL(route.request().url());
    const path = url.pathname.replace('/api/v1/', '/api/');
    if (path === '/api/wizard/draft') {
      if (route.request().method() === 'PUT') draft = route.request().postDataJSON();
      return route.fulfill({ json: draft });
    }
    if (path === '/api/wizard/state') return route.fulfill({ json: { config_present: false, draft_present: true } });
    if (path === '/api/auth/status') return route.fulfill({ json: { setup_complete: false, authenticated: false } });
    if (path === '/api/wizard/geocode') {
      geocode.queries.push(url.searchParams.get('q') ?? '');
      switch (geocode.mode) {
        case 'hits': return route.fulfill({ json: HITS });
        case 'empty': return route.fulfill({ json: [] });
        case 'rate': return route.fulfill({ status: 429, json: { error: 'rate_limited', detail: 'too many geocode requests; wait a minute' } });
        case 'down': return route.fulfill({ status: 502, json: { error: 'geocode_transport_error', detail: 'timeout' } });
      }
    }
    if (path === '/api/location/timezone') return route.fulfill({ json: { timezone: 'America/New_York' } });
    if (path === '/api/location/elevation') return route.fulfill({ json: { elevation_m: 30 } });
    if (path === '/api/info') {
      const response = await route.fetch();
      return route.fulfill({ json: { ...await response.json(), demo: false } });
    }
    if (route.request().method() !== 'GET') return route.fulfill({ status: 403, json: { detail: 'Disabled in location search review' } });
    return route.continue();
  });
  return geocode;
}

async function openLocationStep(page: Page) {
  await page.goto('/setup/location');
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  const box = page.getByPlaceholder('e.g. Springfield, Sydney, or 51.5, -0.1');
  await expect(box).toBeEnabled();
  return box;
}

const latField = (page: Page) => page.getByPlaceholder('e.g. 40.7128');
const lonField = (page: Page) => page.getByPlaceholder('e.g. -74.0060');

test('address search lists sources, fills coordinates on pick, and explains empty, limited and failed lookups', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  const geocode = await sandbox(page);
  const box = await openLocationStep(page);
  const status = page.locator('.geo-status');
  const results = page.locator('.geo-result');

  // Enter searches once, with the query as typed.
  await box.fill('Orlando');
  await box.press('Enter');
  await expect(results).toHaveCount(2);
  expect(geocode.queries).toEqual(['Orlando']);
  await expect(results.nth(0)).toContainText('OpenStreetMap');
  await expect(results.nth(1)).toContainText('US Census');
  await expect(results.nth(1)).toContainText('400 S Orange Ave');

  // A changed query retires the old answer without a new search.
  await box.fill('Orlando FL');
  await expect(results).toHaveCount(0);
  expect(geocode.queries).toHaveLength(1);

  // The Search button works too, and a pick fills the coordinate fields.
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(results).toHaveCount(2);
  await results.nth(1).click();
  await expect(results).toHaveCount(0);
  await expect(latField(page)).toHaveValue('28.53834');
  await expect(lonField(page)).toHaveValue('-81.37924');
  await expect(page.getByPlaceholder('America/New_York')).toHaveValue('America/New_York');

  // No match says so instead of showing nothing.
  geocode.mode = 'empty';
  await box.fill('Nowhere Lane');
  await box.press('Enter');
  await expect(status).toContainText('No matches');
  await expect(status).not.toHaveClass(/geo-status--error/);

  // A rate limit and an outage are errors with words, not silence.
  geocode.mode = 'rate';
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(status).toContainText('Too many searches');
  await expect(status).toHaveClass(/geo-status--error/);
  geocode.mode = 'down';
  await box.press('Enter');
  await expect(status).toContainText('did not answer');

  // Typing clears the message; the box is ready for the next try.
  await box.fill('Orlando');
  await expect(status.locator('.geo-status__text')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Search', exact: true })).toBeEnabled();
  expect(errors).toEqual([]);
});

test('use my location fills the coordinates from the browser', async ({ page, context }) => {
  await sandbox(page);
  await context.grantPermissions(['geolocation'], { origin });
  await context.setGeolocation({ latitude: 28.54, longitude: -81.38 });
  await openLocationStep(page);
  await page.getByRole('button', { name: 'Use my location', exact: true }).click();
  await expect(latField(page)).toHaveValue('28.54');
  await expect(lonField(page)).toHaveValue('-81.38');
  await expect(page.locator('.geo-status')).toContainText("Using this device's location");
  await expect(page.getByPlaceholder('America/New_York')).toHaveValue('America/New_York');
});

// Its own context: a grant, once given, outlives clearPermissions() for a
// position the browser already holds, so a refusal is only reproducible
// where nothing was ever granted.
test('use my location explains a refused permission instead of going quiet', async ({ page }) => {
  await sandbox(page);
  await openLocationStep(page);
  const locate = page.getByRole('button', { name: 'Use my location', exact: true });
  await locate.click();
  await expect(page.locator('.geo-status')).toContainText('permission was denied');
  await expect(page.locator('.geo-status')).toHaveClass(/geo-status--error/);
  await expect(locate).toBeEnabled();
  await expect(latField(page)).toHaveValue('');
});

test('the page allows geolocation to itself', async ({ page }) => {
  const response = await page.request.get('/setup/location');
  expect(response.headers()['permissions-policy']).toContain('geolocation=(self)');
});

test('pick on map drops a pin that fills the coordinates and follows typed values', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  await sandbox(page);
  // No tile request leaves the test: a 1x1 PNG stands in for every OSM tile.
  const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==', 'base64');
  await page.route('https://tile.openstreetmap.org/**', route => route.fulfill({ contentType: 'image/png', body: PNG }));
  await openLocationStep(page);
  const toggle = page.getByRole('button', { name: 'Pick on map', exact: true });
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await toggle.click();
  const canvas = page.locator('.geo-map__canvas');
  await expect(canvas).toHaveAttribute('data-geo-picker-ready', 'yes');
  await expect(page.getByRole('button', { name: 'Hide map', exact: true })).toHaveAttribute('aria-expanded', 'true');
  await expect(page.locator('.geo-pin')).toHaveCount(0);

  // A tap drops the pin and fills the fields through the geo-pick event.
  await canvas.click({ position: { x: 200, y: 160 } });
  await expect(page.locator('.geo-pin')).toHaveCount(1);
  await expect(page.locator('.geo-status')).toContainText('Pin placed');
  await expect(latField(page)).not.toHaveValue('');
  await expect(lonField(page)).not.toHaveValue('');

  // The pin follows the fields: typed coordinates recenter it, no second pick.
  const before = JSON.stringify(await page.locator('.geo-pin').boundingBox());
  await latField(page).fill('28.54');
  await latField(page).blur();
  await lonField(page).fill('-81.38');
  await lonField(page).blur();
  await expect.poll(async () => JSON.stringify(await page.locator('.geo-pin').boundingBox())).not.toBe(before);
  await expect(latField(page)).toHaveValue('28.54');

  await page.getByRole('button', { name: 'Hide map', exact: true }).click();
  await expect(canvas).toHaveCount(0);
  expect(errors).toEqual([]);
});

test('Settings location keeps search controls and results within a phone screen', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await sandbox(page);
  await page.route('**/api/config', route => route.fulfill({ json: {
    deployment: { location: { lat: 28.53834, lon: -81.37924, elevation_m: 30 },
      timezone: 'America/New_York', display_name: 'Example garden' },
  } }));
  await page.goto('/settings/location', { waitUntil: 'domcontentloaded' });
  await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
  const box = page.getByPlaceholder('e.g. Springfield, Sydney, or 51.5, -0.1');
  await expect(box).toBeEnabled();
  const checkWidth = async () => {
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1)).toBe(true);
    for (const control of [box, page.getByRole('button', { name: 'Search', exact: true }),
      page.getByRole('button', { name: 'Use my location', exact: true }),
      page.getByRole('button', { name: 'Pick on map', exact: true })]) {
      const rect = await control.boundingBox();
      expect(rect).not.toBeNull();
      expect(rect!.x).toBeGreaterThanOrEqual(0);
      expect(rect!.x + rect!.width).toBeLessThanOrEqual(391);
    }
    expect((await box.boundingBox())!.width).toBeGreaterThan(150);
  };
  await checkWidth();
  await box.fill('400 S Orange Ave, Orlando, FL 32801');
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.locator('.geo-result')).toHaveCount(2);
  await checkWidth();
  await page.locator('.geo-result').nth(1).click();
  await expect(page.getByRole('spinbutton', { name: 'Latitude', exact: true })).toHaveValue('28.53834');
});
