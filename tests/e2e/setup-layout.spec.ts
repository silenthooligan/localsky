import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.use({ serviceWorkers: 'block', reducedMotion: 'reduce' });
const steps = ['welcome','location','sources','controllers','zones','rules','sensors','llm','notifications','account','review'];

// Only this browser's draft changes. Real persistence and apply/restart remain
// covered by fresh-install.spec on its separate empty-volume instance.
async function sandbox(page: Page) {
  let draft = await (await page.request.get('/api/wizard/draft')).json();
  draft.license_accepted = true;
  draft.config.controllers = [];
  draft.config.zones = {};
  await page.route('**/api/**', async route => {
    const path = new URL(route.request().url()).pathname.replace('/api/v1/', '/api/');
    if (path === '/api/wizard/draft') {
      if (route.request().method() === 'PUT') draft = route.request().postDataJSON();
      return route.fulfill({ json: draft });
    }
    if (path === '/api/wizard/state') return route.fulfill({json:{config_present:false,draft_present:true}});
    if (path === '/api/auth/status') return route.fulfill({json:{setup_complete:false,authenticated:false}});
    if (path === '/api/info') {
      const response=await route.fetch();
      return route.fulfill({json:{...await response.json(),demo:false}});
    }
    if (route.request().method() !== 'GET') return route.fulfill({status:403,json:{detail:'Disabled in layout review'}});
    return route.continue();
  });
}

async function geometry(page: Page) {
  await page.evaluate(() => document.fonts.ready);
  expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
  const issues = await page.evaluate(() => {
    const root = document.querySelector('.sheet--open') ?? document.querySelector('.setup-shell')!;
    const items = [...root.querySelectorAll<HTMLElement>('.btn,.ui-segmented__option,.source-kind-tile')]
      .filter(e => !e.closest('details:not([open])') && e.getClientRects().length && e.getBoundingClientRect().height > 0 && getComputedStyle(e).visibility !== 'hidden');
    const found: string[] = [];
    // Native selects can clip their selected label without reporting overflow.
    // Let the browser size an otherwise identical one-option control naturally.
    for (const select of root.querySelectorAll<HTMLSelectElement>('.appearance-picker select')) {
      const probe = select.cloneNode(false) as HTMLSelectElement;
      probe.append(new Option(select.selectedOptions[0]?.text ?? '', select.value));
      probe.removeAttribute('id');
      Object.assign(probe.style, { position: 'absolute', visibility: 'hidden', width: 'max-content', minWidth: '0', maxWidth: 'none' });
      select.parentElement!.append(probe);
      const needed = probe.getBoundingClientRect().width;
      probe.remove();
      if (needed > select.getBoundingClientRect().width + 1)
        found.push(`Clipped selected option: ${select.selectedOptions[0]?.text}`);
    }
    for (let i=0;i<items.length;i++) {
      const e=items[i], a=e.getBoundingClientRect(), label=e.textContent?.trim();
      if (a.left < -1 || a.right > innerWidth+1 || e.scrollWidth > e.clientWidth+2 || e.scrollHeight > e.clientHeight+2)
        found.push(`Clipped control: ${label}`);
      for (const other of items.slice(i+1)) {
        const b=other.getBoundingClientRect();
        if (Math.min(a.right,b.right)-Math.max(a.left,b.left)>2 && Math.min(a.bottom,b.bottom)-Math.max(a.top,b.top)>2)
          found.push(`Overlapping controls: ${label} / ${other.textContent?.trim()}`);
      }
    }
    return found;
  });
  expect(issues).toEqual([]);
}

for (const style of ['field','slate','classic']) for (const mode of ['light','dark']) for (const width of [390,1440]) {
  test(`wizard steps are readable in ${style} ${mode} at ${width}px`, async ({page}) => {
    test.setTimeout(120_000);
    await page.setViewportSize({width,height:900});
    const errors: string[]=[];
    page.on('pageerror', e=>errors.push(e.message));
    await sandbox(page);
    await page.goto('/setup/location');
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await page.getByRole('combobox',{name:'Theme',exact:true}).selectOption(style);
    await page.getByRole('combobox',{name:'Display mode',exact:true}).selectOption(mode);
    // A real reload proves the controls share the pre-paint persisted state.
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
    await expect(page.getByRole('combobox',{name:'Theme',exact:true})).toHaveValue(style);
    await expect(page.getByRole('combobox',{name:'Display mode',exact:true})).toHaveValue(mode);
    await expect(page.locator('.mobile-tab-bar')).toBeHidden();
    await expect(page.locator('.beta-fb')).toBeHidden();
    for (const step of steps) {
      await page.getByRole('combobox',{name:'Jump to step',exact:true}).selectOption(step);
      await expect(page).toHaveURL(new RegExp(`/setup/${step}$`));
      await expect(page.getByRole('progressbar')).toHaveAttribute('aria-valuenow',String(steps.indexOf(step)+1));
      if (step==='sources') await expect(page.locator('.cloud-row').first()).toBeVisible();
      if (step==='review') await expect(page.locator('.review-row').first()).toBeVisible();
      if (step==='welcome' && width===1440) {
        // Intro copy uses the available card width instead of a short text
        // column that stranded "alone" on its own line on desktop.
        expect(await page.locator('.setup-hero__sub').evaluate(e =>
          e.getBoundingClientRect().width / e.parentElement!.getBoundingClientRect().width
        )).toBeGreaterThan(0.95);
      }
      await geometry(page);
      const axe=await new AxeBuilder({page}).include('.setup-shell').withTags(['wcag2a','wcag2aa','wcag21aa']).analyze();
      expect(axe.violations.filter(v=>['serious','critical'].includes(v.impact ?? ''))).toEqual([]);
      await page.evaluate(()=>window.scrollTo({top:0,behavior:'instant'}));
      await page.screenshot({path:test.info().outputPath(`${style}-${mode}-${width}-${step}.png`),fullPage:true});
    }
    // Choices also reach the app after leaving setup.
    await page.getByRole('link',{name:'LocalSky home',exact:true}).last().click();
    await expect(page.locator('.setup-shell')).toHaveCount(0);
    await expect(page.getByLabel('Display mode',{exact:true})).toHaveValue(mode);
    await expect(page.locator('html')).toHaveAttribute('data-style',style);
    expect(errors).toEqual([]);
  });
}

for (const width of [320,768]) test(`expanded wizard forms and reference guides fit ${width}px`,async ({page})=>{
  test.setTimeout(90_000);
  await page.setViewportSize({width,height:900});
  await sandbox(page);
  await page.goto('/setup/sources');
  await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
  await expect(page.locator('.cloud-row').first()).toBeVisible();
  await page.getByRole('button',{name:'+ Add a weather station',exact:true}).click();
  await geometry(page);
  await page.screenshot({path:test.info().outputPath(`source-editor-${width}.png`),fullPage:true});
  await page.getByRole('combobox',{name:'Jump to step',exact:true}).selectOption('controllers');
  await page.getByRole('button',{name:'+ Add a controller',exact:true}).click();
  await geometry(page);
  await page.screenshot({path:test.info().outputPath(`controller-editor-${width}.png`),fullPage:true});
  await page.getByRole('button',{name:'Cancel',exact:true}).click();
  await page.getByRole('button',{name:'Simulate without hardware',exact:true}).click();
  await expect(page.locator('.setup-shell .cond-row__name').filter({hasText:/^simulated$/})).toBeVisible();
  await page.getByRole('combobox',{name:'Jump to step',exact:true}).selectOption('zones');
  await page.getByText('Grass and ground-cover guide',{exact:true}).click();
  await expect(page.locator('.species-gallery').first()).toBeVisible();
  await geometry(page);
  await page.getByText('Grass and ground-cover guide',{exact:true}).click();
  await page.getByRole('button',{name:/Add your first zone/}).click();
  await expect(page.locator('.sheet--open')).toBeVisible();
  await geometry(page);
  await page.getByPlaceholder('Back Yard').fill('Courtyard');
  await page.getByRole('button',{name:'Add zone',exact:true}).click();
  await expect(page.locator('.setup-zone-list')).toContainText('Courtyard');
  await expect(page.locator('.sheet--open')).toHaveCount(0);
  await page.getByRole('combobox',{name:'Jump to step',exact:true}).selectOption('account');
  await page.getByRole('radio',{name:/From outside my home network/}).check();
  await expect(page.getByLabel('Username',{exact:true})).toBeVisible();
  await geometry(page);
  await page.screenshot({path:test.info().outputPath(`account-editor-${width}.png`),fullPage:true});
  // Auto follows the OS; high contrast stays available in either visual style.
  await page.getByRole('combobox',{name:'Theme',exact:true}).selectOption('slate');
  await page.getByRole('combobox',{name:'Display mode',exact:true}).selectOption('auto');
  await page.emulateMedia({colorScheme:'light'});
  const light = await page.evaluate(()=>getComputedStyle(document.documentElement).getPropertyValue('--bg-deep'));
  await page.emulateMedia({colorScheme:'dark'});
  expect(await page.evaluate(()=>getComputedStyle(document.documentElement).getPropertyValue('--bg-deep'))).not.toBe(light);
  await page.getByRole('combobox',{name:'Display mode',exact:true}).selectOption('hc');
  await expect(page.locator('html')).toHaveAttribute('data-theme','hc');
  await geometry(page);
});

for (const width of [320,1440]) test(`existing setup keeps its choices readable at ${width}px`,async ({page})=>{
  await page.setViewportSize({width,height:900});
  await sandbox(page);
  await page.route('**/api/wizard/state',route=>route.fulfill({json:{config_present:true,draft_present:true}}));
  await page.goto('/setup/location');
  await expect(page.getByRole('heading',{name:'This LocalSky is already set up'})).toBeVisible();
  await expect(page.getByRole('combobox',{name:'Jump to step',exact:true})).toHaveCount(0);
  await page.getByRole('combobox',{name:'Theme',exact:true}).selectOption('slate');
  await page.getByRole('combobox',{name:'Display mode',exact:true}).selectOption('light');
  await geometry(page);
  await expect(page.getByRole('button',{name:'Edit current setup',exact:true})).toBeEnabled();
  await expect(page.getByRole('button',{name:'Resume saved draft',exact:true})).toBeEnabled();
  await page.getByText('Start from scratch',{exact:true}).click();
  await expect(page.getByRole('button',{name:'Create blank draft',exact:true})).toBeEnabled();
  await geometry(page);
  const axe=await new AxeBuilder({page}).include('.setup-shell').withTags(['wcag2a','wcag2aa','wcag21aa']).analyze();
  expect(axe.violations.filter(v=>['serious','critical'].includes(v.impact ?? ''))).toEqual([]);
  await page.screenshot({path:test.info().outputPath(`existing-setup-${width}.png`),fullPage:true});
  await page.getByRole('link',{name:'Back to Settings',exact:true}).click();
  await expect(page.locator('.setup-shell')).toHaveCount(0);
  await expect(page.getByLabel('Display mode',{exact:true})).toHaveValue('light');
});

test('setup waits for its state and keeps the draft untouched when loading fails', async ({page})=>{
  await sandbox(page);
  let release!:()=>void;
  const pending=new Promise<void>(resolve=>release=resolve);
  let fail=true;
  const draftRequests:string[]=[];
  page.on('request',r=>{if(new URL(r.url()).pathname.endsWith('/wizard/draft')) draftRequests.push(r.method());});
  await page.route('**/api/wizard/state',async route=>{
    await pending;
    return fail ? route.fulfill({status:503,json:{error:'unavailable'}})
      : route.fulfill({json:{config_present:true,draft_present:true}});
  });
  await page.goto('/setup');
  await expect(page.locator('html')).toHaveAttribute('data-hydrated','true');
  await expect(page.getByText('Loading your setup…',{exact:true})).toBeVisible();
  expect(draftRequests).toEqual([]);
  release();
  await expect(page.locator('.setup-shell [role=alert]')).toContainText('Could not load your current setup');
  await expect(page.getByRole('combobox',{name:'Jump to step',exact:true})).toHaveCount(0);
  fail=false;
  await page.getByRole('button',{name:'Try again',exact:true}).click();
  await expect(page.getByRole('button',{name:'Resume saved draft',exact:true})).toBeVisible();
  expect(draftRequests).toEqual([]);
  // A failed explicit load stays at the choices, never enters a blank form.
  await page.route('**/api/wizard/seed_current',route=>route.fulfill({status:503,json:{detail:'Current settings could not be loaded.'}}));
  await page.getByRole('button',{name:'Edit current setup',exact:true}).click();
  await expect(page.locator('.setup-shell [role=alert]')).toBeVisible();
  await expect(page.getByRole('button',{name:'Resume saved draft',exact:true})).toBeEnabled();
  await expect(page.getByRole('combobox',{name:'Jump to step',exact:true})).toHaveCount(0);
  expect(draftRequests).toEqual([]);
});
