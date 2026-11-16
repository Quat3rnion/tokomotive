// Focused browser regression. Does not run the screenshot pipeline.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright-core');

async function hook_result_is_hidden_until_signed_sample_completes() {
  const scratch = process.env.TMPDIR;
  assert.ok(scratch, 'Set TMPDIR to the test fixture directory');
  fs.mkdirSync(scratch, { recursive: true });
  const dir = fs.mkdtempSync(path.join(scratch, 'ui-'));
  const mock = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ decision: 'continue' }));
    });
  });
  await new Promise(resolve => mock.listen(0, '127.0.0.1', resolve));
  const reserve = http.createServer();
  await new Promise(resolve => reserve.listen(0, '127.0.0.1', resolve));
  const port = reserve.address().port;
  await new Promise(resolve => reserve.close(resolve));
  const password = crypto.randomBytes(16).toString('hex');
  const config = path.join(dir, 'seed.toml');
  fs.writeFileSync(config, `[server]\nmode='simple'\nadmin_password='${password}'\n`);
  const log = fs.openSync(path.join(dir, 'server.log'), 'w');
  const env = { ...process.env, TMPDIR: dir, TOKIO_WORKER_THREADS: '4' };
  for (const key of ['TOKOMOTIVE_CONFIG', 'TOKOMOTIVE_ADMIN_PASSWORD', 'TOKOMOTIVE_SECRET_KEY', 'TOKOMOTIVE_METRICS_TOKEN']) delete env[key];
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
      assert.ok(Date.now() < deadline, 'Local test server did not become ready');
      assert.equal(server.exitCode, null, 'Local test server exited');
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
    const created = await context.request.post(`${base}/admin/hooks`, {
      form: {
        name: 'browser-hook', url: `http://127.0.0.1:${mock.address().port}/hook`,
        pre_request: 'on', enabled: 'on', allow_private: 'on',
      },
      headers: { origin: base },
    });
    assert.ok(created.ok());
    const id = (await created.text()).match(/href="\/admin\/hooks#hook-(\d+)"/)[1];
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${base}/admin/hooks`);
    const result = page.locator(`#test-${id}`);
    assert.equal(await result.isVisible(), false, 'Empty Test-result box must be hidden');
    await page.getByRole('button', { name: 'Test signed sample' }).click();
    await result.waitFor({ state: 'visible' });
    await page.waitForFunction(selector => document.querySelector(selector).textContent.includes('"outcome":"continue"'), `#test-${id}`);
    const decision = JSON.parse(await result.textContent());
    assert.equal(decision.status, 200);
    assert.equal(decision.decision.decision, 'continue');
    assert.ok(decision.latency_ms >= 0);
    assert.deepEqual(errors, []);
    console.log('hook_result_is_hidden_until_signed_sample_completes: PASS');
  } finally {
    if (browser) await browser.close();
    server.kill('SIGTERM');
    await new Promise(resolve => {
      if (server.exitCode !== null) return resolve();
      const timeout = setTimeout(() => { server.kill('SIGKILL'); resolve(); }, 3000);
      server.once('exit', () => { clearTimeout(timeout); resolve(); });
    });
    await new Promise(resolve => mock.close(resolve));
    fs.closeSync(log);
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

hook_result_is_hidden_until_signed_sample_completes().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
