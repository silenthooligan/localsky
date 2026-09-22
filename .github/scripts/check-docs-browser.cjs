/* Validate guide navigation and layout using the existing browser-test lockfile. */
const { chromium } = require('../../tests/e2e/node_modules/@playwright/test');
const { default: AxeBuilder } = require('../../tests/e2e/node_modules/@axe-core/playwright');
const fs = require('node:fs');

async function checkTableValues(page, name, theme, width) {
  const problems = await page.locator('main table code, main table .ls-value').evaluateAll(values => values.flatMap(value => {
    const range = document.createRange();
    range.selectNodeContents(value);
    const rects = [...range.getClientRects()].filter(rect => rect.width > 0);
    const lines = new Set(rects.map(rect => Math.round(rect.top)));
    const cell = value.closest('td, th').getBoundingClientRect();
    const clipped = rects.some(rect => rect.left < cell.left - 1 || rect.right > cell.right + 1);
    return lines.size > 1 || clipped ? [value.textContent] : [];
  }));
  if (problems.length) throw new Error(`${name}: split or clipped table values at ${width}px: ${problems.join(', ')}`);

  if (name === 'authentication') {
    const table = page.locator('main table').first();
    const values = await table.locator('tbody td:first-child').allTextContents();
    if (values.join(',') !== 'required,disabled') throw new Error('Access-policy values changed');
    const fits = await table.evaluate(element => {
      const wrapper = element.closest('.table-wrapper');
      return wrapper.scrollWidth <= wrapper.clientWidth + 1;
    });
    if (!fits) throw new Error(`Access-policy details should wrap without scrolling at ${width}px`);
    if (width <= 390) {
      const lines = await table.locator('tbody td:nth-child(2)').first().evaluate(element => {
        const range = document.createRange();
        range.selectNodeContents(element);
        return new Set([...range.getClientRects()].map(rect => Math.round(rect.top))).size;
      });
      if (lines < 2) throw new Error('Access-policy description did not wrap');
    }
    await table.screenshot({ path: `docs-browser-proof/access-policy-${theme}-${width}.png` });
  }

  // Long endpoint values must remain readable through keyboard scrolling.
  if (name === 'api-irrigation' && width === 320) {
    const wrapper = page.locator('main .table-wrapper').filter({ hasText: 'GET /export?days=365&format=csv' });
    if (!await wrapper.evaluate(element => element.scrollWidth > element.clientWidth)) throw new Error('Expected wide endpoint table');
    if (await wrapper.getAttribute('tabindex') !== '0') throw new Error('Wide table is not keyboard reachable');
    await wrapper.focus();
    await page.keyboard.press('ArrowRight');
    await page.waitForFunction(() => document.activeElement.scrollLeft > 0);
    await wrapper.screenshot({ path: `docs-browser-proof/wide-table-${theme}-${width}.png` });
  }
}

(async () => {
  const browser = await chromium.launch();
  const checks = [];
  const failures = [];
  fs.mkdirSync('docs-browser-proof', { recursive: true });
  try {
    for (const viewport of [{ width: 1440, height: 1000 }, { width: 390, height: 844 }, { width: 320, height: 740 }]) {
      const context = await browser.newContext({ viewport });
      const page = await context.newPage();
      await page.route('**/*', route => {
        if (new URL(route.request().url()).hostname !== '127.0.0.1') return route.abort();
        return route.continue();
      });
      for (const theme of ['ayu', 'light']) {
        await page.goto('http://127.0.0.1:8765/index.html');
        await page.evaluate(value => localStorage.setItem('mdbook-theme', value), theme);
        for (const name of ['introduction', 'developers', 'api-quickstart', 'api-weather', 'irrigation-engine', 'backup-restore', 'authentication', 'api-irrigation', 'soil-textures']) {
          const response = await page.goto(`http://127.0.0.1:8765/${name}.html`);
          if (response.status() !== 200) throw new Error(`${name}: HTTP ${response.status()}`);
          await page.locator('main h1').waitFor();
          if (!await page.evaluate(value => document.documentElement.classList.contains(value), theme)) throw new Error(`Theme did not apply: ${theme}`);
          const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 2);
          if (overflow) throw new Error(`${name}: page overflow at ${viewport.width}px`);
          await checkTableValues(page, name, theme, viewport.width);
          if (name === "introduction" || name === "developers") await page.screenshot({ path: `docs-browser-proof/${name}-${theme}-${viewport.width}.png`, fullPage: true });
          const results = await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
          if (results.violations.length) {
            fs.writeFileSync(`docs-browser-proof/axe-${name}-${theme}-${viewport.width}.json`, JSON.stringify(results.violations, null, 2));
            failures.push(`${name} (${theme}, ${viewport.width}px): ${results.violations.map(v => v.id).join(', ')}`);
          }
          if (name === 'introduction' || name === 'developers') {
            await page.screenshot({ path: `docs-browser-proof/${name}-${theme}-${viewport.width}.png`, fullPage: true });
            const cards = page.locator('.ls-doc-path');
            if (await cards.count() !== 3) throw new Error(`${name}: expected three task cards`);
            await cards.first().click();
            if (!page.url().includes(name === 'introduction' ? 'getting-started' : 'api-quickstart')) throw new Error('Task card navigation failed');
          }
          checks.push({ page: name, theme, width: viewport.width, overflow: false, accessibility: results.violations.length ? 'failed' : 'passed' });
        }
      }
      await context.close();
    }
    fs.writeFileSync('docs-browser-proof/results.json', JSON.stringify(checks, null, 2));
    if (failures.length) throw new Error(failures.join('\n'));
    console.log(`${checks.length} guide layout/accessibility checks passed.`);
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error.message); process.exitCode = 1; });
