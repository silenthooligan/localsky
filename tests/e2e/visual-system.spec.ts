import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import visualDemo from "./fixtures/visual-demo.json";
import type { Page } from "@playwright/test";

test.use({ serviceWorkers: "block" });

for (const width of [390, 1440]) {
  test(`Settings search finds controls and retention confirms deletion at ${width}px`, async ({page}) => {
    await page.setViewportSize({width, height: 900});
    let saved = { persistence: {retention_days: 90, runs_retention_days: 0}, untouched: "preserve" };
    const writes: any[] = [];
    await page.route(/\/api\/(v1\/)?config$/, async route => {
      if (route.request().method() === 'PUT') {
        saved = route.request().postDataJSON(); writes.push(saved);
        return route.fulfill({json: {ok: true, restart_required: false}});
      }
      return route.fulfill({json: saved});
    });
    await page.goto('/settings');
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await page.getByRole('searchbox', {name:'Find a setting'}).fill('retention');
    await page.getByRole('link',{name:'History retention',exact:false}).click();
    await expect(page.getByRole('heading',{name:'History retention',exact:true})).toBeVisible();
    const sensor = page.getByLabel('Sensor readings (days)',{exact:true});
    const watering = page.getByLabel('Watering history (days)',{exact:true});
    await expect(sensor).toHaveValue('90');
    await expect(watering).toHaveValue('0');
    expect((await sensor.boundingBox())!.height).toBeGreaterThanOrEqual(40);
    expect((await watering.boundingBox())!.width).toBeGreaterThan(200);
    await watering.fill('365');
    await page.getByRole('button',{name:'Save retention',exact:true}).click();
    await expect(page.getByRole('dialog')).toContainText('permanently deleted');
    expect(writes).toHaveLength(0);
    await page.getByRole('button',{name:'Cancel',exact:true}).click();
    expect(writes).toHaveLength(0);
    await page.getByRole('button',{name:'Save retention',exact:true}).click();
    await page.getByRole('button',{name:'Save shorter limits',exact:true}).click();
    await expect(page.getByRole('status').filter({hasText:'Saved.'})).toBeVisible();
    expect(writes).toHaveLength(1);
    expect(saved).toEqual({persistence:{retention_days:90,runs_retention_days:365},untouched:'preserve'});
    await watering.fill('0');
    await page.getByRole('button',{name:'Save retention',exact:true}).click();
    await expect.poll(()=>writes.length).toBe(2);
    await expect(page.getByRole('dialog')).not.toBeVisible();
    await page.getByRole('searchbox',{name:'Find a setting'}).fill('offline');
    await page.getByRole('link',{name:/Source freshness/}).click();
    await expect(page.locator('#source-freshness')).toBeVisible();
    expect(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth)).toBe(false);
    await page.screenshot({path:test.info().outputPath(`settings-search-${width}.png`),fullPage:true});
  });
}

test('source freshness distinguishes healthy, disabled, unknown and failed requests', async ({page}) => {
  await page.clock.install();
  let failed=false;
  const states=[['active','Active'],['watching','Watching'],['standby','Standby'],['falling_through','Backup in use'],['offline','Offline'],['future_state','Unknown']];
  await page.route(/\/api\/(v1\/)?health(?:\?.*)?$/, route=> failed
    ? route.fulfill({status:503,json:{error:'unavailable'}})
    : route.fulfill({json:{status:'ok',version:'0.9.4',config_present:true,sources:[
      ...states.map(([status])=>({id:status,kind:'nws',enabled:true,status,last_seen_epoch:Math.floor(Date.now()/1000)-5})),
      {id:'disabled',kind:'nws',enabled:false,status:'offline',last_seen_epoch:null},
      {id:'unlocated',kind:'nws',enabled:true,status:'watching',note:'not polled: no location is configured yet'}
    ]}}));
  await page.goto('/settings?section=advanced');
  const list=page.locator('.source-status-list');
  await expect(list.locator('.source-status-pill')).toHaveText([...states.map(s=>s[1]),'Off','Needs location']);
  await expect(list.locator('.source-status-pill-offline')).toHaveCount(1);
  failed=true;
  await page.clock.fastForward(31_000);
  await expect(page.locator('#source-freshness')).toContainText('Source status is unavailable');
  await expect(list).not.toBeVisible();
  failed=false;
  await page.clock.fastForward(31_000);
  await expect(list.locator('.source-status-pill-offline')).toHaveCount(1);
  await page.goto('/settings');
  await expect(page.locator('.settings-overview__row-meta').filter({hasText:/^Active$/})).toBeVisible();
});

for (const theme of ['light','dark']) {
  for (const width of [390,1440]) {
    test(`Slate style preserves meaning and readability in ${theme} at ${width}px`,async ({page})=>{
      test.setTimeout(60_000);
      await page.setViewportSize({width,height:900});
      // Measure readable surfaces, not the route's intermediate entrance opacity.
      await page.emulateMedia({reducedMotion:'reduce'});
      await page.addInitScript(t=>{
        if (localStorage.getItem('theme') === null) localStorage.setItem('theme',t);
        if (localStorage.getItem('style') === null) localStorage.setItem('style','slate');
      },theme);
      for (const path of ['/','/irrigation','/history','/settings?section=history','/settings?section=theme']) {
        if (path === '/settings?section=history') {
          await page.route(/\/api\/(v1\/)?config$/, route => route.fulfill({json: {
            persistence: {retention_days: 90, runs_retention_days: 0}
          }}));
        }
        await page.goto(path,{waitUntil:'domcontentloaded'});
        await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
        await expect(page.locator('html')).toHaveAttribute('data-style','slate');
        await page.evaluate(()=>document.fonts.ready);
        if (path === '/settings?section=history') await expect(page.getByRole('button',{name:'Save retention',exact:true})).toBeVisible();
        expect(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth)).toBe(false);
        const colors=await page.evaluate(()=>{const s=getComputedStyle(document.documentElement);return ['--chart-rain','--accent-warn','--accent-good','--chart-et'].map(v=>s.getPropertyValue(v).trim());});
        expect(new Set(colors).size).toBe(4);
        const axe=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa']).analyze();
        expect(axe.violations.filter(v=>v.impact==='serious'||v.impact==='critical')).toEqual([]);
        await page.screenshot({path:test.info().outputPath(`slate-${theme}-${width}-${path.replace(/[^a-z]/g,'')||'weather'}.png`),fullPage:true});
      }
      await page.getByRole('button',{name:/^Field Green /}).click();
      await expect(page.locator('html')).toHaveAttribute('data-style','field');
      await page.reload();
      await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
      await expect(page.locator('html')).toHaveAttribute('data-style','field');
      if (theme==='dark') await expect(page.locator('html')).not.toHaveAttribute('data-theme');
      else await expect(page.locator('html')).toHaveAttribute('data-theme',theme);
      await page.getByRole('button',{name:/^Slate /}).click();
      await page.getByRole('button',{name:/^High contrast/}).click();
      await expect(page.locator('html')).toHaveAttribute('data-theme','hc');
      // The production stylesheet minifies six-digit black to its CSS shorthand.
      expect(await page.locator('html').evaluate(e=>getComputedStyle(e).getPropertyValue('--bg-deep').trim())).toMatch(/^#(?:000|000000)$/);
    });
  }
}

// Exercise real hydrated measurements at the widths that exposed clipped
// weather cards. Synthetic instance only; no controller commands or settings writes.
for (const theme of ["light", "dark", "hc", "auto"]) {
  for (const width of [320, 390, 900, 1280, 1440, 1920]) {
    test(`weather remains readable at ${width}px in ${theme}`, async ({ page }) => {
      test.setTimeout(45_000);
      await page.setViewportSize({ width, height: 900 });
      await page.emulateMedia({ colorScheme: "light", reducedMotion: "reduce" });
      await page.addInitScript(t => {
        localStorage.setItem("theme", t);
        localStorage.setItem("nerd_mode", "true");
      }, theme);
      await page.goto("/", { waitUntil: "domcontentloaded" });
      await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
      await expect(page.locator(".hourly-svg")).toBeVisible();
      await expect(page.locator(".forecast-hourly .chart-key")).toContainText("Rain chance (%)");
      await expect(page.locator(".hourly-svg")).toContainText("%");
      await page.evaluate(() => document.fonts.ready);
      const clipped = await page.evaluate(() => {
        const failures: string[] = [];
        if (document.documentElement.scrollWidth > innerWidth) failures.push("page overflow");
        for (const card of document.querySelectorAll(".weather-observations > .panel, .weather-grid > .hero")) {
          const bounds = card.getBoundingClientRect();
          for (const element of card.querySelectorAll(".big-number, .kv .v, .compass, .strike-radar")) {
            const range = document.createRange();
            range.selectNode(element);
            if ([...range.getClientRects()].some(r => r.width && (r.left < bounds.left || r.right > bounds.right + 1))) {
              failures.push(element.textContent ?? element.className);
            }
          }
        }
        return failures;
      });
      expect(clipped, "measurement or graphic clipped by its card").toEqual([]);
      if (width >= 761) {
        const cards = await page.locator(".weather-observations > .panel").evaluateAll(nodes => nodes.map(n => { const b=n.getBoundingClientRect(); return {y:b.y,height:b.height}; }));
        expect(cards).toHaveLength(6);
        for (let i=0; i<cards.length; i+=2) {
          expect(Math.abs(cards[i].y-cards[i+1].y)).toBeLessThan(1);
          expect(Math.abs(cards[i].height-cards[i+1].height)).toBeLessThan(1);
        }
      }
      const hero = (await page.locator(".weather-grid > .hero").boundingBox())!;
      const radar = (await page.locator(".weather-grid > .radar").boundingBox())!;
      const observations = (await page.locator(".weather-observations").boundingBox())!;
      if (width >= 1101) {
        expect(radar.x).toBeGreaterThan(hero.x + hero.width);
        expect((await page.locator("#radar-map").boundingBox())!.height).toBeGreaterThanOrEqual(600);
        expect(Math.abs(radar.y - hero.y)).toBeLessThan(2);
        expect(radar.height).toBeGreaterThan(observations.height);
      } else {
        expect(radar.y).toBeGreaterThanOrEqual(hero.y + hero.height);
        expect(observations.y).toBeGreaterThan(radar.y + radar.height);
      }
      const results = await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
      expect(results.violations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) }))).toEqual([]);
      await page.screenshot({ path: test.info().outputPath(`${theme}-${width}-weather.png`), fullPage: true });
    });
  }
}

for (const width of [320, 1440]) {
  test(`header appearance and detail controls stay synchronized at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/settings/theme", { waitUntil: "domcontentloaded" });
    await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
    const picker = page.getByRole("combobox", { name: "Display mode" });
    await picker.selectOption("light");
    await expect(page.locator('.theme-card').filter({ has: page.locator('.theme-card__label', { hasText: /^Light$/ }) })).toHaveAttribute("aria-pressed", "true");
    await page.locator('.theme-card').filter({ has: page.locator('.theme-card__label', { hasText: /^Dark$/ }) }).click();
    await expect(picker).toHaveValue("dark");
    await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute("content", "#091a17");
    await picker.selectOption("auto");
    await page.emulateMedia({ colorScheme: "light" });
    await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute("content", "#eeefe5");
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute("content", "#091a17");
    await picker.selectOption("hc");
    await page.getByRole("button", { name: "Nerd", exact: true }).click();
    await expect(page.locator("html")).toHaveAttribute("data-nerd", "true");
    await page.reload({ waitUntil: "domcontentloaded" });
    await expect(picker).toHaveValue("hc");
    await expect(page.getByRole("button", { name: "Nerd", exact: true })).toHaveAttribute("aria-pressed", "true");
    expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
    await page.screenshot({ path: test.info().outputPath(`header-${width}.png`) });
  });
}

async function fixedReadings(page: Page, change: (responses: Record<string, any>) => void) {
  const responses = structuredClone(visualDemo.responses) as Record<string, any>;
  change(responses);
  await page.clock.setFixedTime(new Date(visualDemo.now));
  await page.addInitScript(() => localStorage.setItem("nerd_mode", "true"));
  const streams: Record<string, string> = {
    "/api/stream": "/api/snapshot", "/api/irrigation/stream": "/api/irrigation/snapshot",
    "/api/forecast/stream": "/api/forecast/snapshot",
  };
  await page.route("**/api/**", async route => {
    const request = route.request();
    if (request.method() !== "GET") return route.abort();
    const url = new URL(request.url());
    const key = url.pathname.replace("/api/v1/", "/api/") + url.search;
    const response = responses[streams[key] ?? key];
    if (response === undefined) return route.continue();
    return streams[key]
      ? route.fulfill({ contentType: "text/event-stream", body: `event: snapshot\ndata: ${JSON.stringify(response)}\n\n` })
      : route.fulfill({ json: response });
  });
}

for (const [style, theme, width] of [
  ['field', 'light', 320], ['field', 'dark', 1440],
  ['slate', 'dark', 390], ['slate', 'light', 1440],
  ['classic', 'light', 390], ['classic', 'hc', 1440],
] as const) {
  test(`sky evidence is readable and keyboard accessible ${style} ${theme} ${width}`, async ({page}) => {
    await page.setViewportSize({width, height:900});
    await page.emulateMedia({reducedMotion:'reduce'});
    const errors: string[] = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.addInitScript(({style, theme}) => {
      localStorage.setItem('style',style); localStorage.setItem('theme',theme);
    },{style,theme});
    await fixedReadings(page, responses => {
      const current = responses['/api/snapshot'];
      current.sky = {...current.sky, condition:'overcast', cover_basis:'measured_sunlight', cloud_cover_pct:null};
      current.wet_bulb_f = null;
      current.feels_like_f = null;
      responses['/api/location'] = {lat:69.65, lon:18.96, located:true, zoom:8};
      const forecast = responses['/api/forecast/snapshot'];
      // Polar winter: even midday is night. Browser timezone is irrelevant.
      for (const [i,hour] of forecast.hourly.entries()) {
        hour.time_epoch = Date.parse('2026-12-21T00:00:00Z')/1000 + i*3600;
        hour.weather_code = 2;
      }
    });
    await page.goto('/');
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await expect(page.locator('.hero-condition')).toHaveText('Overcast');
    const evidence=page.locator('.hero-sky-evidence');
    const summary=evidence.locator('summary');
    await summary.focus(); await page.keyboard.press('Enter');
    await expect(evidence).toHaveAttribute('open','');
    await expect(evidence.locator('p')).toContainText('not a cloud-cover measurement');
    await expect(page.locator('.hero-source')).toContainText('Temperature via');
    await expect(page.locator('.hero').getByText('Temperature unavailable',{exact:true})).toHaveCount(2);
    // The moon path is used for a partly-cloudy polar winter hour.
    await expect(page.locator('.hourly-glyph').first()).toHaveAttribute('aria-label','Partly cloudy, nighttime');
    await expect(page.locator('.hourly-glyph').nth(12)).toHaveAttribute('aria-label','Partly cloudy, nighttime');
    await page.evaluate(()=>document.fonts.ready);
    expect(await page.evaluate(()=>document.documentElement.scrollWidth > innerWidth)).toBe(false);
    const axe=await new AxeBuilder({page}).include('.hero').withTags(['wcag2a','wcag2aa','wcag21aa']).analyze();
    expect(axe.violations).toEqual([]);
    expect(errors).toEqual([]);
    await page.screenshot({path:test.info().outputPath(`sky-${style}-${theme}-${width}.png`),fullPage:true});
  });
}

for (const [width, style, theme, scale] of [
  [320, 'field', 'light', 'f'], [390, 'field', 'dark', 'c'],
  [1440, 'field', 'light', 'f'], [1440, 'slate', 'dark', 'c'],
  [390, 'classic', 'hc', 'c'], [320, 'classic', 'dark', 'f'],
] as const) {
  test(`weather outlook and temperature anatomy ${width} ${style} ${theme} ${scale}`, async ({ page }) => {
    const errors: string[] = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.setViewportSize({ width, height: 900 });
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await fixedReadings(page, responses => {
      const forecast = responses['/api/forecast/snapshot'];
      forecast.daily[0].cape_max_jkg = 2200;
      forecast.daily[0].apparent_temp_max_f = 104;
      forecast.hourly[4].wet_bulb_f = 79;
      forecast.hourly[5].wind_gusts_mph = 40;
      const current = responses['/api/snapshot'];
      current.air_temp_f = scale === 'c' ? -4 : 100;
      current.feels_like_f = 99;
      current.wet_bulb_f = 79;
    });
    await page.addInitScript(({ style, theme, scale }) => {
      localStorage.setItem('style', style);
      localStorage.setItem('theme', theme);
      localStorage.setItem('units_system', 'custom');
      localStorage.setItem('units_temp', scale);
    }, { style, theme, scale });
    await page.goto('/', { waitUntil: 'domcontentloaded' });
    await expect(page.locator('html')).toHaveAttribute('data-hydrated', 'true');
    const storm = page.locator('[data-condition="storm"]');
    const heat = page.locator('[data-condition="heat"]');
    await expect(storm).toContainText('Conditions favor thunderstorm growth.');
    await expect(storm.locator('details')).not.toHaveAttribute('open');
    await expect(storm.getByText('CAPE', { exact: true })).not.toBeVisible();
    const temperatures = heat.locator('.temperature-value');
    await expect(temperatures).toHaveCount(2);
    await expect(temperatures.first().locator('.temperature-value__number')).toHaveText(scale === 'c' ? '40' : '104');
    await expect(temperatures.last().locator('.temperature-value__number')).toHaveText(scale === 'c' ? '26' : '79');
    await expect(page.locator('.hero-temp .temperature-value__number')).toHaveText(scale === 'c' ? '-20' : '100');
    await expect(temperatures.first().locator('.sr-only')).toHaveText(scale === 'c' ? '40 degrees Celsius' : '104 degrees Fahrenheit');
    for (const temperature of await page.locator('.temperature-value').all()) {
      await expect(temperature.locator('.temperature-value__degree')).toHaveText('°');
      await expect(temperature.locator('.temperature-value__scale')).toHaveText(scale.toUpperCase());
    }
    await page.evaluate(() => document.fonts.ready);
    const readingTops = await temperatures.evaluateAll(nodes => nodes.map(n => n.getBoundingClientRect().top));
    expect(Math.abs(readingTops[0] - readingTops[1]), 'wrapped labels must not offset comparable readings').toBeLessThan(1);
    const clipped = await page.locator('.temperature-value').evaluateAll(nodes => nodes.flatMap(node => {
      const parent = node.closest('.condition-temperature, .panel, .hero')!.getBoundingClientRect();
      const value = node.getBoundingClientRect();
      return value.left < parent.left || value.right > parent.right + 1 ? [node.textContent] : [];
    }));
    expect(clipped).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
    await page.locator('.condition-cards').screenshot({ path: test.info().outputPath('weather-outlook.png') });
    await storm.getByText('Forecast details', { exact: true }).focus();
    await page.keyboard.press('Enter');
    await expect(storm.getByText('CAPE', { exact: true })).toBeVisible();
    const accessibility = await new AxeBuilder({ page }).include('.condition-cards').analyze();
    expect(accessibility.violations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) }))).toEqual([]);
    expect(errors).toEqual([]);
  });
}

for (const width of [320, 390]) {
  test(`a long watering state leaves a readable explanation at ${width}px`, async ({page}) => {
    await page.setViewportSize({width,height:900});
    await page.emulateMedia({reducedMotion:'reduce'});
    await fixedReadings(page,responses=>{
      const s=responses['/api/irrigation/snapshot'];
      s.ha_reachable=true;
      s.zones.forEach((z:any)=>{z.running=false;z.running_known=true;z.ledger_running=false;});
      s.next_run_epoch=0;
      s.next_run_state='no_water_planned';
      s.skip_check.will_skip=false;
    });
    await page.goto('/',{waitUntil:'domcontentloaded'});
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await page.evaluate(()=>document.fonts.ready);
    const strip=page.locator('.home-watering-verdict');
    await expect(strip).toContainText('NO WATERING PLANNED');
    await expect(strip.locator('> span').nth(1)).toBeVisible();
    await expect(strip.locator('> span').nth(2)).toBeVisible();
    const word=(await strip.locator('> span').nth(1).boundingBox())!;
    const reason=(await strip.locator('> span').nth(2).boundingBox())!;
    expect(reason.y).toBeGreaterThanOrEqual(word.y+word.height);
    expect(reason.width).toBeGreaterThan(200);
    expect((await strip.boundingBox())!.height).toBeLessThan(180);
    expect(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth)).toBe(false);
    await strip.screenshot({path:test.info().outputPath(`watering-strip-${width}.png`)});
  });
}

test("a recorded skip cannot label tomorrow's run and late pressure stays in the plot", async ({ page }) => {
  await fixedReadings(page, responses => {
    const s = responses["/api/irrigation/snapshot"];
    s.zones.forEach((z: any) => { z.running = false; z.running_known = true; });
    s.next_run_state = "at";
    s.next_run_epoch = s.seven_day_verdicts[1].time_epoch;
    s.next_run_day_offset = 1;
    s.seven_day_verdicts[1].verdict = "run";
    responses["/api/snapshot"].pressure_trend_inhg = [[7200, 30.02], [3600, 29.97], [7199, 29.96], [7260, 30.03]];
  });
  await page.goto("/", { waitUntil: "domcontentloaded" });
  const strip = page.locator(".home-watering-verdict");
  await expect(strip).toContainText("WATER");
  await expect(strip).toContainText("Next run");
  await expect(strip).not.toContainText("SKIP");
  await expect(page.locator(".pressure .trend-label")).toHaveText("rising");
  const bounds = await page.locator(".pressure .sparkline-path").evaluate((e: SVGPathElement) => {
    const box = e.getBBox();
    return { x: box.x, y: box.y, width: box.width, height: box.height };
  });
  expect(bounds.x).toBeGreaterThanOrEqual(1);
  expect(bounds.y).toBeGreaterThanOrEqual(1);
  expect(bounds.width).toBeGreaterThan(190);
  expect(bounds.x + bounds.width).toBeLessThan(200);
  expect(bounds.y + bounds.height).toBeLessThan(60);
});

for (const theme of ["light", "dark", "hc", "auto"]) {
  test(`thresholds and covered budgets have distinct color and text in ${theme}`, async ({ page }) => {
    await page.addInitScript(t => localStorage.setItem("theme", t), theme);
    await fixedReadings(page, responses => {
      const s = responses["/api/irrigation/snapshot"];
      s.forecast.rain_today_tempest_in = 0.05; // Exact boundary, not only above.
      s.skip_check.already_wet_in = 0.05;
      s.forecast.rain_tomorrow_in = 0.01;
      s.forecast.rain_7day_in = 1.0;
      s.forecast.rain_3day_in = null;
      s.forecast.eto_today_mm = 1;
    });
    await page.goto("/irrigation", { waitUntil: "domcontentloaded" });
    const met = page.locator(".rain-bar").filter({ has: page.locator(".rain-bar-label", { hasText: /^Measured today$/ }) });
    const below = page.locator(".rain-bar").filter({ has: page.locator(".rain-bar-label", { hasText: /^Tomorrow$/ }) });
    const missing = page.locator(".rain-bar").filter({ has: page.locator(".rain-bar-label", { hasText: /^Next 3 days$/ }) });
    await expect(met.locator(".rain-bar-state")).toContainText("Threshold met");
    await expect(below.locator(".rain-bar-state")).toContainText("Below threshold");
    await expect(missing.locator(".rain-bar-state")).toContainText("Unknown");
    await expect(missing.locator(".rain-bar-fill")).toHaveCount(0);
    const beforeColor = await met.locator(".rain-bar-before").evaluate(e => getComputedStyle(e).backgroundColor);
    const belowColor = await below.locator(".rain-bar-before").evaluate(e => getComputedStyle(e).backgroundColor);
    expect(beforeColor).toEqual(belowColor); // Reaching the line must not recolor earlier rain.
    const at = (await met.locator(".rain-bar-excess").boundingBox())!;
    expect(at.width).toBeLessThan(1); // Exact boundary: no fictitious excess.
    const over = page.locator(".rain-bar-above").filter({ has: page.locator(".rain-bar-excess") });
    const segments = await over.evaluateAll(bars => bars.map(b => {
      const a=b.querySelector('.rain-bar-before')!, x=b.querySelector('.rain-bar-excess')!, t=b.querySelector('.rain-bar-threshold')!;
      return { before:getComputedStyle(a).backgroundColor, after:getComputedStyle(x).backgroundColor, edge:a.getBoundingClientRect().right, tick:t.getBoundingClientRect().x+1, width:x.getBoundingClientRect().width };
    }));
    expect(segments.some(s => s.width > 5)).toBe(true);
    for (const segment of segments) {
      expect(segment.before).not.toEqual(segment.after);
      expect(Math.abs(segment.edge-segment.tick)).toBeLessThan(2);
    }
    await expect(page.locator(".fc-balance .chart-key")).toContainText("Rain beyond budget");
    const surplus = page.locator(".balance-bar__surplus");
    expect((await surplus.boundingBox())!.width).toBeGreaterThan(1);
    expect(await surplus.evaluate(e => getComputedStyle(e).backgroundColor)).not.toEqual(belowColor);
    const slash = page.locator(".sprinkler-off-slash").first();
    await expect(slash).toBeVisible();
    expect(await slash.evaluate(e => getComputedStyle(e).stroke)).not.toEqual(await slash.evaluate(e => getComputedStyle(e.parentElement!).stroke));
    await page.locator(".next-run-hero").screenshot({ path: test.info().outputPath(`${theme}-held-icon.png`) });
    await page.locator(".forecast-panel").screenshot({ path: test.info().outputPath(`${theme}-threshold-states.png`) });
  });
}

test("watering history can be explored with a keyboard", async ({ page }) => {
  await page.goto("/history", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
  const chart = page.locator(".ui-line-chart").first();
  await expect(chart).toBeVisible();
  await expect(chart.locator(".ui-line-chart__legend")).toContainText("Watered");
  const plot = chart.locator(".ui-line-chart__plot");
  await plot.focus();
  await plot.press("Home");
  const first = await chart.locator(".ui-line-chart__tip-head").innerText();
  await plot.press("End");
  await expect(chart.locator(".ui-line-chart__tip-head")).not.toHaveText(first);
  await expect(chart.locator(".ui-line-chart__tip-val")).toContainText("min");
  await plot.press("Escape");
  await expect(chart.locator(".ui-line-chart__tip")).toHaveCount(0);
});

for (const theme of ["light", "dark"]) {
  for (const width of [390, 1440]) {
    for (const path of ["/week", "/sensors", "/simulator", "/rules", "/irrigation/decisions", "/settings/theme", "/settings/help", "/settings?section=devices"]) {
      test(`${path} fits ${width}px in ${theme}`, async ({ page }) => {
        const errors: string[] = [];
        page.on("pageerror", error => errors.push(error.message));
        await page.setViewportSize({ width, height: 900 });
        await page.addInitScript(t => localStorage.setItem("theme", t), theme);
        await page.goto(path, { waitUntil: "domcontentloaded" });
        await expect(page.locator("html")).toHaveAttribute("data-hydrated", "true");
        await page.evaluate(() => document.fonts.ready);
        await page.waitForTimeout(1500);
        expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), "page overflow").toBe(false);
        expect(errors).toEqual([]);
        await page.screenshot({ path: test.info().outputPath(`${theme}-${width}-screen.png`), fullPage: true });
      });
    }
  }
}

for (const [theme, width] of [["light", 1440], ["dark", 390]] as const) {
  test(`confirmed automatic cycles show completion and daily sessions connect in ${theme}`, async ({ page }) => {
    await page.setViewportSize({width,height:900});
    await page.addInitScript(t => localStorage.setItem("theme", t), theme);
    let date = "";
    let zoneSlug = "";
    await fixedReadings(page, responses => {
      const s=responses["/api/irrigation/snapshot"];
      zoneSlug = s.zones[0].slug;
      s.ha_reachable=true;
      s.zones.forEach((z:any) => {z.running=false;z.running_known=true;z.ledger_running=false;});
      const now=s.last_refresh_epoch;
      date = new Intl.DateTimeFormat("en-CA",{timeZone:s.timezone,year:"numeric",month:"2-digit",day:"2-digit"}).format(new Date(now*1000));
      const runs=[0,300].map(offset => ({zone:s.zones[0].slug,start_epoch:now-900+offset,duration_s:120,source:"ha_refresher",status:"completed",skip_reason:null,session_id:`smart:${date}:${s.zones[0].slug}`}));
      const window={from_epoch:now-86400,to_epoch:now,runs,daily:[{date_local:date,epoch:now-1000,kind:"scheduled",zones:[{zone:s.zones[0].slug,name:s.zones[0].name,planned_seconds:240,reason:"The soil water balance called for watering",reason_code:"water_needed",water_need:"Refill the modeled deficit"}]}]};
      for(const key of Object.keys(responses)) if(key.startsWith("/api/irrigation/history")) responses[key]=window;
      responses["/api/irrigation/history?days=2"]=window;
      responses["/api/irrigation/history?days=0"]=window;
    });
    await page.goto("/irrigation",{waitUntil:"domcontentloaded"});
    const hero=page.locator('.irrigation-overview');
    await expect(hero).toHaveClass(/hero-complete/);
    await expect(hero.locator('h1')).toHaveText('Watered today');
    await expect(hero).toContainText('Watered 4 min total');
    await expect(hero).not.toContainText('Morning recorded');
    await hero.screenshot({path:test.info().outputPath(`${theme}-completed.png`)});
    await page.getByRole('link',{name:"Today's activity →",exact:true}).click();
    const day=page.locator('.daily-log__day').first();
    await day.locator('summary').click();
    await expect(day).toContainText('4.0 min watered');
    await expect(day).toContainText('includes automatic watering');
    await page.getByRole('link',{name:"View this day's sessions →",exact:true}).click();
    await expect(page).toHaveURL(new RegExp(`date=${date}`));
    await expect(page.locator('.hist-panel__title').first()).toHaveText('Run log');
    await expect(page.getByRole('button',{name:'Run log',exact:true})).toHaveAttribute('aria-pressed','true');
    expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
    await page.screenshot({path:test.info().outputPath(`${theme}-session-history.png`),fullPage:true});
    await page.goto(`/zones/${zoneSlug}`,{waitUntil:"domcontentloaded"});
    const today=page.locator('.zone-detail__stats .stat-tile').filter({has:page.locator('.stat-tile__label',{hasText:/^Today$/})});
    await expect(today.locator('.stat-tile__value')).toHaveText('4.0');
    await expect(today.locator('.stat-tile__unit')).toHaveText('min');
    await page.route('**/api/irrigation/history?days=30',route=>route.fulfill({status:503,json:{error:'History unavailable'}}));
    await page.reload({waitUntil:'domcontentloaded'});
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await expect(today.locator('.stat-tile__value')).toHaveText('-');
    await expect(today.locator('.stat-tile__unit')).toHaveCount(0);
  });
}
