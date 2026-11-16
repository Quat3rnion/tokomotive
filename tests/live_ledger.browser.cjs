// Run after cargo build. Uses only a new local database and a mock Provider.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright-core');

async function main() {
  const scratch = process.env.TMPDIR;
  assert.ok(scratch, 'Set TMPDIR to the test fixture directory');
  fs.mkdirSync(scratch, { recursive: true });
  const dir = fs.mkdtempSync(path.join(scratch, 'browser-'));
  const mock = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ model: 'test-model', choices: [], usage: { prompt_tokens: 12, completion_tokens: 7 } }));
    });
  });
  await new Promise(resolve => mock.listen(0, '127.0.0.1', resolve));
  const reserve = http.createServer();
  await new Promise(resolve => reserve.listen(0, '127.0.0.1', resolve));
  const port = reserve.address().port;
  await new Promise(resolve => reserve.close(resolve));
  const config = path.join(dir, 'seed.toml');
  fs.writeFileSync(config, `[server]\n[[providers]]\nname='mock'\nkind='openai'\nbase_url='http://127.0.0.1:${mock.address().port}'\napi_key='private-upstream-key'\n[[users]]\nname='alice'\nkeys=['sk-browser']\n`);
  const log = fs.openSync(path.join(dir, 'server.log'), 'w');
  const env = { ...process.env, TMPDIR: dir, TOKIO_WORKER_THREADS: '4' };
  for (const key of ['TOKOMOTIVE_CONFIG', 'TOKOMOTIVE_ADMIN_PASSWORD', 'TOKOMOTIVE_SECRET_KEY', 'TOKOMOTIVE_METRICS_TOKEN']) { delete env[key]; }
  const server = spawn(path.resolve('target/debug/tokomotive'), ['--config', config, '--listen', `127.0.0.1:${port}`, '--database', `sqlite://${dir}/db`], { env, stdio: ['ignore', log, log] });
  let browser;
  try {
    browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || `${process.env.HOME}/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome`, headless: true });
    const context = await browser.newContext({ reducedMotion: 'reduce' });
    const base = `http://127.0.0.1:${port}`;
    const readyBy = Date.now() + 120000;
    for (;;) {
      assert.equal(server.exitCode, null, fs.readFileSync(path.join(dir, 'server.log'), 'utf8'));
      try { if ((await context.request.get(`${base}/healthz`, { timeout: 1000 })).ok()) { break; } } catch (e) { if (Date.now() > readyBy) { throw e; } }
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    const setup = await context.request.post(`${base}/setup`, { form: { username: 'owner', password: 'browser-password', mode: 'full' }, maxRedirects: 0 });
    assert.equal(setup.status(), 303);
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    page.on('console', m => { if (m.type() === 'error') { errors.push(m.text()); } });
    const count = () => page.locator('#ledger-rows > tr[id^="e"]').count();
    const send = async () => {
      const r = await context.request.post(`${base}/v1/chat/completions`, { headers: { authorization: 'Bearer sk-browser' }, data: { model: 'mock/test-model', messages: [] } });
      assert.equal(r.status(), 200, await r.text());
    };
    const waitRows = n => page.waitForFunction(n => document.querySelectorAll('#ledger-rows > tr[id^="e"]').length === n, n);
    await page.goto(`${base}/admin/ledger`);
    assert.equal(await page.locator('#ledger-live').getAttribute('aria-pressed'), 'false');
    await page.click('#ledger-live');
    await page.waitForFunction(() => document.getElementById('ledger-live-status').textContent.startsWith('Live,'));
    // Pause before the first event: resume must use the page's initial cursor.
    await page.click('#ledger-pause');
    await send();
    await page.waitForTimeout(150);
    assert.equal(await count(), 0);
    await page.click('#ledger-pause');
    await waitRows(1);
    await page.locator('#ledger-rows > tr[id^="e"] button[data-toggle]').first().click();
    assert.equal(await page.locator('#ledger-rows > tr.edit-row').first().isVisible(), true);
    await page.click('#ledger-pause');
    await send();
    await page.waitForTimeout(150);
    assert.equal(await count(), 1);
    await page.click('#ledger-pause');
    await waitRows(2);
    await page.click('[data-theme-set="light"]');
    assert.equal(await page.locator('html').getAttribute('data-theme'), 'light');
    await send();
    await waitRows(3);
    // Real committed requests exercise the 200-entry cap and paired Details removal.
    for (let i = 0; i < 201; i += 1) { await send(); }
    await waitRows(200);
    await page.waitForFunction(() => document.querySelector('#ledger-rows > tr[id^="e"]').id === 'e204');
    assert.equal(await page.locator('#ledger-rows > tr.edit-row').count(), 200);
    await page.reload();
    assert.equal(await page.locator('#ledger-live').getAttribute('aria-pressed'), 'true');
    await page.click('#ledger-live');
    await page.reload();
    assert.equal(await page.locator('#ledger-live').getAttribute('aria-pressed'), 'false');
    assert.deepEqual(errors, []);
    console.log('PASS live Ledger: toggle, pause/resume (including before first event), rows, Details, 200-entry cap, theme, persistence, no console errors.');
  } finally {
    if (browser) { await browser.close(); }
    server.kill('SIGTERM');
    await new Promise(resolve => server.exitCode !== null ? resolve() : server.once('exit', resolve));
    mock.closeAllConnections();
    await new Promise(resolve => mock.close(resolve));
    fs.closeSync(log);
    console.log(`Browser check data: ${dir}`);
  }
}
main().catch(e => { console.error(e); process.exitCode = 1; });
