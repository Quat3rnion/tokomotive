const { chromium } = require('playwright-core');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

const base = process.env.SHOTS_BASE_URL || 'http://127.0.0.1:18831';
const out = process.env.SHOTS_CAPTURE_DIR;
const manifest = JSON.parse(fs.readFileSync(path.join(__dirname, 'manifest.json'), 'utf8'));
const width = 1440;

async function capture() {
  assert(out, 'SHOTS_CAPTURE_DIR is required');
  assert(process.env.CHROMIUM_PATH, 'CHROMIUM_PATH is required');
  fs.mkdirSync(out, { recursive: true });
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH });
  try {
    const context = await browser.newContext({
      viewport: { width, height: 900 }, deviceScaleFactor: 2,
      colorScheme: 'dark', reducedMotion: 'reduce',
    });
    const page = await context.newPage();
    const errors = [];
    page.on('console', message => {
      if (message.type() === 'error') errors.push(`${page.url()} ${message.text()}`);
    });
    page.on('pageerror', error => errors.push(`${page.url()} ${error.message}`));
    const go = async route => {
      const response = await page.goto(base + route, { waitUntil: 'networkidle' });
      assert(response && response.ok(), `GET ${route}: ${response && response.status()}`);
      assert.equal(new URL(page.url()).pathname, new URL(base + route).pathname, `redirect from ${route}`);
      await page.evaluate(() => document.fonts.ready);
    };
    await go('/admin/login');
    await page.fill('input[name=password]', process.env.SHOTS_PASSWORD || 'demo-password-2026');
    await Promise.all([page.waitForURL(base + '/admin'), page.press('input[name=password]', 'Enter')]);

    const box = (selector, text, ancestor, last = false) => page.evaluate(args => {
      const [selector, text, ancestor, last] = args;
      const all = [...document.querySelectorAll(selector)].filter(el => !text || el.textContent.trim().startsWith(text));
      let element = last ? all.at(-1) : all[0];
      if (!element) throw new Error(`Missing element: ${selector} ${text}`);
      if (ancestor) element = element.closest(ancestor);
      const rect = element.getBoundingClientRect();
      return { top: Math.floor(rect.top + window.scrollY), bottom: Math.ceil(rect.bottom + window.scrollY) };
    }, [selector, text, ancestor, last]);
    const full = () => page.evaluate(() => Math.ceil(document.documentElement.scrollHeight));
    const mode = async value => {
      const response = await page.request.post(base + '/admin/settings/mode', {
        form: { mode: value }, headers: { Origin: base }, maxRedirects: 0,
      });
      assert.equal(response.status(), 303, 'mode change');
      await go('/admin/settings'); // Clear the one-time flash message.
    };
    const overview = async () => [0, (await box('div.panel[aria-labelledby$="-h"]', 'Model')).bottom + 10];
    const recipes = {
      overview,
      'overview-simple': async entry => {
        await mode('simple');
        await go(entry.page);
        return overview();
      },
      ledger: async () => [0, 840],
      routes: async () => [0, (await box('h2', 'smart', 'section')).bottom + 10],
      providers: async () => [0, (await box('h2', 'codex-cli', 'section')).bottom + 10],
      models: async () => [0, (await box('h2', 'Default Markup', 'section')).bottom + 10],
      pager: async () => [0, (await box('nav.pager', null, null, true)).bottom + 24],
      full: async () => [0, await full()],
      signin: async () => {
        // Use the local paste panel. Do not start a real OAuth/device flow.
        await page.evaluate(() => {
          const section = [...document.querySelectorAll('h2')].find(h => h.textContent.trim().startsWith('codex-cli')).closest('section');
          return htmx.ajax('POST', '/admin/providers/codex-cli/oauth/paste', { target: section.querySelector('.signin'), swap: 'innerHTML' });
        });
        await page.waitForSelector('#cb-oauth-codex-cli');
        await page.evaluate(() => { document.activeElement.blur(); window.scrollTo(0, 0); });
        const bounds = await box('h2', 'codex-cli', 'section');
        return [Math.max(0, bounds.top - 16), bounds.bottom + 16];
      },
      'add-provider': async () => {
        await page.click('button.np-open');
        await page.locator('#np-search').fill('kimi');
        await page.waitForSelector('[role=listbox] [role=option]');
        const bounds = await page.evaluate(() => {
          const list = document.querySelector('[role=listbox]:not([hidden])').getBoundingClientRect();
          const panel = (document.querySelector('main h1') || document.getElementById('add-provider-panel')).getBoundingClientRect();
          return { top: Math.floor(panel.top + window.scrollY), bottom: Math.ceil(Math.max(panel.bottom, list.bottom) + window.scrollY) };
        });
        return [Math.max(0, bounds.top - 24), bounds.bottom + 64];
      },
      'add-provider-form': async () => {
        const bounds = await box('#add-provider-panel');
        return [Math.max(0, bounds.top - 60), bounds.bottom + 24];
      },
    };
    const captured = new Set();
    for (const entry of manifest.captures) {
      assert(/^[a-z0-9-]+\.png$/.test(entry.image), `Invalid image ${entry.image}`);
      assert(!captured.has(entry.image), `Duplicate capture ${entry.image}`);
      assert(recipes[entry.recipe], `Missing recipe ${entry.recipe}`);
      await go(entry.page);
      const [top, bottom] = await recipes[entry.recipe](entry);
      await page.screenshot({ path: path.join(out, entry.image), fullPage: true,
        clip: { x: 0, y: top, width, height: Math.min(bottom, await full()) - top } });
      captured.add(entry.image);
      console.log(`captured ${entry.image}`);
      if (entry.recipe === 'overview-simple') await mode('full');
    }
    assert.equal(captured.size, manifest.captures.length, 'Missing capture');
    assert.deepEqual(errors, [], 'Browser console/page errors');
  } finally {
    await browser.close();
  }
}

capture().catch(error => { console.error(error); process.exitCode = 1; });
