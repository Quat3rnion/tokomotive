// Focused form regression. Does not run the screenshot pipeline.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright-core');

async function exporter_fields_and_gov_policy_work_in_the_browser() {
  assert.ok(process.env.TMPDIR, 'Set TMPDIR to the test fixture directory');
  const dir = fs.mkdtempSync(path.join(process.env.TMPDIR, 'export-ui-'));
  const calls = [];
  const mock = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', chunk => chunks.push(chunk));
    req.on('end', () => {
      calls.push({ path: req.url, headers: req.headers, body: JSON.parse(Buffer.concat(chunks)) });
      res.writeHead(202);
      res.end();
    });
  });
  await new Promise(resolve => mock.listen(0, '127.0.0.1', resolve));
  const reserve = http.createServer();
  await new Promise(resolve => reserve.listen(0, '127.0.0.1', resolve));
  const port = reserve.address().port;
  await new Promise(resolve => reserve.close(resolve));
  const password = crypto.randomBytes(16).toString('hex');
  const key = crypto.randomBytes(32).toString('hex');
  const config = path.join(dir, 'seed.toml');
  fs.writeFileSync(config, `[server]\nmode='simple'\nadmin_password='${password}'\n`);
  const log = fs.openSync(path.join(dir, 'server.log'), 'w');
  const env = { ...process.env, TMPDIR: dir, TOKIO_WORKER_THREADS: '4' };
  for (const name of ['TOKOMOTIVE_CONFIG', 'TOKOMOTIVE_ADMIN_PASSWORD', 'TOKOMOTIVE_SECRET_KEY', 'TOKOMOTIVE_METRICS_TOKEN']) delete env[name];
  const binary = process.env.TOKOMOTIVE_BINARY || path.resolve('target/debug/tokomotive');
  const server = spawn(binary, ['--config', config, '--listen', `127.0.0.1:${port}`, '--database', `sqlite://${dir}/db`], { env, stdio: ['ignore', log, log] });
  let browser;
  try {
    browser = await chromium.launch({
      executablePath: process.env.CHROMIUM_PATH || `${process.env.HOME}/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome`,
      env: { ...process.env, TMPDIR: dir },
      headless: true,
    });
    const context = await browser.newContext({ reducedMotion: 'reduce' });
    const base = `http://127.0.0.1:${port}`;
    const deadline = Date.now() + 30000;
    for (;;) {
      assert.ok(Date.now() < deadline, 'Local server did not become ready');
      assert.equal(server.exitCode, null, fs.readFileSync(path.join(dir, 'server.log'), 'utf8'));
      try {
        if ((await context.request.get(`${base}/healthz`, { timeout: 1000 })).ok()) break;
      } catch (error) {
        if (Date.now() > deadline) throw error;
      }
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok((await context.request.post(`${base}/admin/login`, {
      form: { username: 'admin', password }, headers: { origin: base },
    })).ok());
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${base}/admin/settings`);
    let form = page.locator('[data-exporter-form]').last();
    assert.equal(await form.locator('[name=include_bodies]').isChecked(), false);
    await form.locator('[name=site]').selectOption('gov');
    assert.equal(await form.locator('[name=spans]').isDisabled(), true);
    assert.equal(await form.locator('[name=spans]').isChecked(), false);
    await form.locator('[name=site]').selectOption('EU1');
    assert.equal(await form.locator('[name=spans]').isEnabled(), true);
    await form.locator('[name=kind]').selectOption('maxim');
    assert.equal(await form.locator('[name=repo_id]').isVisible(), true);
    assert.equal(await form.locator('[name=repo_id]').getAttribute('required'), '');
    assert.equal(await form.locator('[name=site]').isDisabled(), true);
    await form.locator('[name=kind]').selectOption('datadog');
    await form.locator('[name=site]').selectOption('gov');
    await form.locator('[name=metrics]').check();
    await form.locator('[name=name]').fill('browser-exporter');
    await form.locator('[name=api_key]').fill(key);
    await form.locator('summary').click();
    await form.locator('[name=endpoint]').fill(`http://127.0.0.1:${mock.address().port}`);
    await form.locator('[name=allow_private]').check();
    await form.getByRole('button', { name: 'Add exporter', exact: true }).click();
    await page.waitForURL('**/admin/settings#exporters');
    form = page.locator('#exporter-1 [data-exporter-form]');
    await form.waitFor();
    assert.equal(await form.locator('[name=spans]').isDisabled(), true);
    assert.equal(await form.locator('[name=api_key]').inputValue(), '');
    assert.equal((await page.content()).includes(key), false);
    await page.getByRole('button', { name: 'Test sample', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('ex1-result').textContent.includes('"outcome":"sent"'));
    assert.equal(calls.length, 1);
    assert.equal(calls[0].path, '/api/v2/series');
    assert.equal(calls[0].headers['dd-api-key'], key);
    await form.locator('[name=kind]').selectOption('maxim');
    await form.locator('[name=repo_id]').fill('browser-repository');
    await form.getByRole('button', { name: 'Save exporter', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('ex1-result').textContent === 'Not tested.');
    await page.getByRole('button', { name: 'Test sample', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('ex1-result').textContent.includes('"outcome":"sent"'));
    assert.equal(calls.length, 2);
    assert.equal(calls[1].path, '/v1/logs/batch');
    assert.equal(calls[1].headers['x-maxim-api-key'], key);
    assert.equal(calls[1].body.repoId, 'browser-repository');
    assert.equal(calls[1].body.logs[0].input, undefined);
    assert.ok((await context.request.post(`${base}/admin/settings/exporters`, {
      headers: { origin: base },
      form: {
        name: 'reference-exporter', kind: 'datadog', site: 'US1', spans: 'on',
        api_key: 'env:DEMO_UNUSED_EXPORT_KEY', include_bodies: 'on',
        endpoint: `http://127.0.0.1:${mock.address().port}`, allow_private: 'on',
      },
    })).ok());
    const adminPassword = crypto.randomBytes(16).toString('hex');
    assert.ok((await context.request.post(`${base}/admin/settings/accounts`, {
      headers: { origin: base }, form: { username: 'export-admin', role: 'admin', password: adminPassword },
    })).ok());
    const adminContext = await browser.newContext({ reducedMotion: 'reduce' });
    assert.ok((await adminContext.request.post(`${base}/admin/login`, {
      headers: { origin: base }, form: { username: 'export-admin', password: adminPassword },
    })).ok());
    const adminPage = await adminContext.newPage();
    adminPage.on('pageerror', error => errors.push(error.message));
    await adminPage.goto(`${base}/admin/settings`);
    let locked = adminPage.locator('#exporter-2 [data-exporter-form]');
    for (const selector of ['select[name=kind]', 'select[name=site]', '[name=api_key]', 'input[type=checkbox][name=include_bodies]']) {
      assert.equal(await locked.locator(selector).isDisabled(), true, `Owner-only: ${selector}`);
    }
    await locked.locator('[name=sample_rate]').fill('0.5');
    await locked.getByRole('button', { name: 'Save exporter', exact: true }).click();
    await adminPage.waitForURL('**/admin/settings#exporters');
    locked = adminPage.locator('#exporter-2 [data-exporter-form]');
    assert.equal(await locked.locator('[name=sample_rate]').inputValue(), '0.5');
    assert.equal(await locked.locator('input[type=checkbox][name=include_bodies]').isChecked(), true,
      'Admin saves must preserve the Owner body permission');
    assert.deepEqual(errors, []);
    console.log('exporter_fields_and_gov_policy_work_in_the_browser: PASS');
  } finally {
    if (browser) await browser.close();
    server.kill('SIGTERM');
    await new Promise(resolve => {
      if (server.exitCode !== null) resolve();
      else server.once('exit', resolve);
    });
    await new Promise(resolve => mock.close(resolve));
    fs.closeSync(log);
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
exporter_fields_and_gov_policy_work_in_the_browser().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
