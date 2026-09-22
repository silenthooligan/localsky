/* Validate guide navigation and layout using the existing browser-test lockfile. */
const { chromium } = require('../../tests/e2e/node_modules/@playwright/test');
const { default: AxeBuilder } = require('../../tests/e2e/node_modules/@axe-core/playwright');
const fs = require('node:fs');
const path = require('node:path');

(async () => {
  const browser = await chromium.launch();
  const checks = [];
  fs.mkdirSync('docs-browser-proof', { recursive: true });
  try {
    for (const viewport of [{ width: 1440, height: 1000 }, { width: 390, height: 844 }]) {
      const page = await browser.newPage({ viewport });
      await page.route('**/*', route => {
        if (new URL(route.request().url()).hostname !== '127.0.0.1') return route.abort();
        return route.continue();
      });
      for (const theme of ['ayu', 'light']) {
        await page.addInitScript(value => localStorage.setItem('mdbook-theme', value), theme);
        for (const name of ['introduction', 'developers', 'api-quickstart', 'api-weather', 'irrigation-engine', 'backup-restore']) {
          const response = await page.goto(`http://127.0.0.1:8765/${name}.html`);
          if (response.status() !== 200) throw new Error(`${name}: HTTP ${response.status()}`);
          await page.locator('main h1').waitFor();
          const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 2);
          if (overflow) throw new Error(`${name}: page overflow at ${viewport.width}px`);
          const results = await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
          if (results.violations.length) {
            fs.writeFileSync(`docs-browser-proof/axe-${name}-${theme}-${viewport.width}.json`, JSON.stringify(results.violations, null, 2));
            throw new Error(`${name}: accessibility failures: ${results.violations.map(v => v.id).join(', ')}`);
          }
          if (name === 'introduction' || name === 'developers') {
            await page.screenshot({ path: `docs-browser-proof/${name}-${theme}-${viewport.width}.png`, fullPage: true });
            const cards = page.locator('.ls-doc-path');
            if (await cards.count() !== 3) throw new Error(`${name}: expected three task cards`);
            await cards.first().click();
            if (!page.url().includes(name === 'introduction' ? 'getting-started' : 'api-quickstart')) throw new Error('Task card navigation failed');
          }
          checks.push({ page: name, theme, width: viewport.width, overflow: false, accessibility: 'passed' });
        }
      }
      await page.close();
    }
    fs.writeFileSync('docs-browser-proof/results.json', JSON.stringify(checks, null, 2));
    console.log(`${checks.length} guide layout/accessibility checks passed.`);
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error.message); process.exitCode = 1; });
