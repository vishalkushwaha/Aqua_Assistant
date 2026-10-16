// Browser test for the Workflows page, run against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root, or the release build)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/workflows.e2e.mjs
//
// Every UI action is confirmed through the API, so "it was saved" means the database changed, not only
// the page. The test creates workflows named "e2e-…" and deletes every one of them at the end; it never
// touches a workflow it did not create. Steps are "compose text" steps only: nothing outside Aqua's own
// database is changed.

import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');

const UI = process.env.AQUA_UI_URL ?? 'http://127.0.0.1:1420';
const CORE = 'http://127.0.0.1:8767';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};

const api = async (method, url, body) => {
  const r = await fetch(`${CORE}${url}`, {
    method,
    headers: { Authorization: `Bearer ${uiToken}`, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: r.status, json: await r.json().catch(() => null) };
};

const mine = async () => (await api('GET', '/api/v1/workflows')).json.workflows.filter((w) => w.name.startsWith('e2e-'));
const until = async (what, fn, ms = 20000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`timed out: ${what}`);
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  await api('GET', '/api/v1/workflows').then((r) => {
    if (r.status !== 200) throw new Error(`aqua-core is not answering (${r.status})`);
  });
  for (const w of await mine()) await api('POST', `/api/v1/workflows/${w.id}/delete`);

  await page.goto(UI);
  // A full-screen first-run overlay (model onboarding) can cover the app; it is not what is tested here.
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^Workflows$/ }).click();
  await page.getByRole('heading', { name: 'Workflows', exact: true }).waitFor();
  check('the Workflows tab opens and shows the scheduler', await page.getByText(/Scheduler running/).isVisible());

  // ---- create ---------------------------------------------------------------------------
  await page.getByRole('button', { name: /New workflow/ }).click();
  await page.getByPlaceholder('Morning market report').fill('e2e-compose');
  await page.locator('select').first().selectOption('manual');
  await page.getByLabel('Step type').selectOption('set');
  await page.getByPlaceholder(/Report for/).fill('hello {{now.date}}');
  await page.getByRole('button', { name: /Add step/ }).click();
  await page.getByLabel('Step type').nth(1).selectOption('set');
  await page.getByLabel('Step id').nth(1).fill('again');
  await page.getByPlaceholder(/Report for/).nth(1).fill('again: {{steps.step1.output}}');
  await page.getByRole('button', { name: 'Create workflow' }).click();
  await page.getByText('e2e-compose').first().waitFor();
  const created = (await mine()).find((w) => w.name === 'e2e-compose');
  check('creating a workflow saves it with both steps', !!created && created.steps.length === 2 && created.trigger.type === 'manual', JSON.stringify(created));

  // ---- validation errors come back in plain words ---------------------------------------------
  await page.getByRole('button', { name: /New workflow/ }).click();
  await page.getByPlaceholder('Morning market report').fill('e2e-leaky');
  await page.getByLabel('Step type').selectOption('tool');
  await page.getByPlaceholder('fs.read').fill('web.get');
  await page.locator('textarea').first().fill('{"url":"https://x.example","password":"hunter2"}');
  await page.getByRole('button', { name: 'Create workflow' }).click();
  const alert = await page.getByRole('alert').textContent();
  check('a password in a step is refused with an explanation', /plain text/.test(alert ?? '') && !(await mine()).some((w) => w.name === 'e2e-leaky'), alert ?? '');
  await page.locator('textarea').first().fill('not json');
  await page.getByRole('button', { name: 'Create workflow' }).click();
  check('bad JSON is caught before it is sent', /JSON object/.test((await page.getByRole('alert').textContent()) ?? ''));
  await page.getByRole('button', { name: 'Cancel' }).click();

  // ---- run and read the history ------------------------------------------------------------
  const card = page.locator('div', { has: page.getByText('e2e-compose', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Run now/ }) }).last();
  await card.getByRole('button', { name: /Run now/ }).click();
  await until('a run to be recorded', async () => (await api('GET', `/api/v1/workflows/${created.id}/runs`)).json.runs.some((r) => r.status === 'succeeded'));
  await card.getByRole('button', { name: /History/ }).click();
  await page.getByRole('heading', { name: /History: e2e-compose/ }).waitFor();
  await page.getByRole('button', { name: /succeeded/ }).first().click();
  const today = new Date().toISOString().slice(0, 10);
  await page.getByText(`hello ${new Date().getFullYear()}`, { exact: false }).first().waitFor({ timeout: 5000 }).catch(() => {});
  const body = await page.locator('body').innerText();
  check('the history shows each step and the data passed between them', body.includes('again: hello') && /1\. step1/.test(body) && /2\. again/.test(body), body.slice(0, 300));
  check('the first step used the date', body.includes(today) || /hello \d{4}-\d{2}-\d{2}/.test(body));
  await page.getByRole('button', { name: /Close/ }).click();

  // ---- switch off, edit, delete ---------------------------------------------------------------
  await page.getByRole('switch', { name: 'Enable e2e-compose' }).click();
  await until('workflow to be disabled', async () => !(await api('GET', `/api/v1/workflows/${created.id}`)).json.enabled);
  check('the switch turns the workflow off in the database', true);

  await card.getByRole('button', { name: /Edit/ }).click();
  await page.getByPlaceholder('Morning market report').fill('e2e-renamed');
  await page.locator('select').first().selectOption('daily');
  await page.locator('input[type="time"]').fill('07:30');
  await page.getByText(/Next runs:/).waitFor();
  const preview = await until('the schedule preview', async () => {
    const t = (await page.getByText(/Next runs:/).textContent()) ?? '';
    return t.includes('…') ? null : t;
  });
  check('a schedule shows its next three run times before saving', preview.split('·').length === 3 && /7:30/.test(preview), preview);
  await page.getByRole('button', { name: 'Save changes' }).click();
  await page.getByText('e2e-renamed').first().waitFor();
  const edited = (await api('GET', `/api/v1/workflows/${created.id}`)).json;
  check('editing saves the new name and schedule', edited.name === 'e2e-renamed' && edited.trigger.type === 'schedule' && edited.trigger.cron === '30 7 * * *', JSON.stringify(edited.trigger));
  check('the card describes the schedule in words', await page.getByText(/Every day at 07:30/).first().isVisible());

  await page.locator('div', { has: page.getByText('e2e-renamed', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Delete/ }) }).last().getByRole('button', { name: /Delete/ }).click();
  await until('workflow to be deleted', async () => (await api('GET', `/api/v1/workflows/${created.id}`)).status === 404);
  check('deleting removes the workflow and its history', (await api('GET', `/api/v1/workflows/${created.id}/runs`)).status === 404);
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const w of await mine().catch(() => [])) await api('POST', `/api/v1/workflows/${w.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
