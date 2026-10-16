// Browser test for scheduling flows, against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows-schedule.e2e.mjs
//
// The flow is a trigger and an in-app notification, so a real run finishes at once. The schedule is a
// real one in the real database; the test deletes it and the flow at the end and touches nothing else.

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

const until = async (what, fn, ms = 20000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out: ${what}`);
};

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));

const flowDef = {
  name: 'e2e-schedule',
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'scheduled hello' } },
  ],
  edges: [{ from: 'start', to: 'say' }],
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1100 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  const made = await api('POST', '/api/v1/flows', flowDef);
  if (made.status !== 201) throw new Error(`could not create the flow: ${JSON.stringify(made.json)}`);
  const flowId = made.json.id;
  const schedules = async () => (await api('GET', `/api/v1/flows/${flowId}/schedules`)).json.schedules;
  const runs = async () => (await api('GET', `/api/v1/flows/${flowId}/runs`)).json.runs;

  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-schedule').first().waitFor();
  await page.locator('div', { has: page.getByText('e2e-schedule', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Open/ }) }).last().getByRole('button', { name: /Open/ }).click();
  await page.locator('[data-node="say"]').waitFor();

  // ---- writing a schedule in words ----------------------------------------------------------------------------
  await page.getByRole('tab', { name: 'Schedule' }).click();
  await page.getByTestId('schedule-panel').waitFor();
  await page.getByLabel('When should it run').fill('sometime soon');
  await page.getByTestId('schedule-create').click();
  check('a sentence with no time is refused in words', /could not read a time/.test(await page.getByRole('alert').textContent()));
  await page.getByLabel('When should it run').fill('every weekday at 9:30');
  await page.getByLabel('Time zone').selectOption('Asia/Kolkata');
  await page.getByRole('button', { name: 'Show next times' }).click();
  const preview = await page.getByTestId('schedule-preview').innerText();
  check('the next five times are shown before saving', (await page.getByTestId('schedule-preview').locator('li').count()) === 5 && /Asia\/Kolkata/.test(preview), preview);
  check('nothing is saved by previewing', (await schedules()).length === 0);
  await page.getByLabel('If a time is missed').selectOption('run_now');
  await page.getByTestId('schedule-create').click();
  const row = page.getByTestId('schedule-row').first();
  await row.waitFor();
  const [s] = await schedules();
  check('the schedule is stored with its zone, cron and missed-run choice', s.when.cron === '30 9 * * 1-5' && s.timezone === 'Asia/Kolkata' && s.misfire === 'run_now', JSON.stringify(s));
  check('the row says when it runs next', /Next:/.test(await row.innerText()) && /scheduled/.test(await row.innerText()));

  // ---- run now leaves a notification ---------------------------------------------------------------------------
  await row.getByRole('button', { name: /Run now/ }).click();
  await until('the run to finish', async () => (await runs())[0]?.status === 'succeeded');
  check('running it now starts a real run', (await runs())[0].status === 'succeeded');
  await row.getByRole('button', { name: 'History' }).click();
  check('the history says it ran because you asked', await row.getByText(/you ran it now/).waitFor({ timeout: 8000 }).then(() => true, () => false), await row.innerText());

  // ---- switching off and on -------------------------------------------------------------------------------------
  await row.getByRole('button', { name: /Switch off/ }).click();
  await until('switched off', async () => (await schedules())[0].enabled === false);
  check('a switched-off schedule says so', await row.getByText('switched off').waitFor({ timeout: 8000 }).then(() => true, () => false));
  await row.getByRole('button', { name: 'Switch on' }).click();
  await until('switched on', async () => (await schedules())[0].enabled === true);

  // ---- a campaign: it stops by itself ---------------------------------------------------------------------------------
  await page.getByLabel('When should it run').fill('every weekday at 9 for 2 weeks');
  await page.getByLabel('Inputs that take turns').fill('not json');
  await page.getByTestId('schedule-create').click();
  check('inputs that take turns must be valid JSON, and it says so', /not valid JSON/.test(await page.getByRole('alert').textContent()));
  await page.getByLabel('Inputs that take turns').fill('[{"topic": "agents"}, {"topic": "memory"}]');
  await page.getByLabel('Stop after this many runs').fill('4');
  await page.getByRole('button', { name: 'Show next times' }).click();
  check('the preview mentions how long it goes on', /for 2 weeks/.test(await page.getByTestId('schedule-preview').innerText()), await page.getByTestId('schedule-preview').innerText());
  await page.getByTestId('schedule-create').click();
  await until('the campaign to be stored', async () => (await schedules()).length === 2);
  const camp = (await schedules()).find((x) => x.campaign?.max_runs === 4);
  check('it is stored as a campaign with a last day, a number of runs and inputs in turn', camp && camp.campaign.end_on && camp.campaign.series.length === 2 && camp.runs_started === 0, JSON.stringify(camp));
  const line = page.getByTestId('campaign-line');
  await line.waitFor();
  check('the row shows the campaign\'s progress', /0 of 4 runs started/.test(await line.innerText()) && /2 inputs take turns/.test(await line.innerText()), await line.innerText());
  await page.getByTestId('schedule-row').filter({ hasText: 'campaign' }).first().getByRole('button', { name: 'History' }).click().catch(() => undefined);
  await page.getByTestId('campaign-summary').first().waitFor({ timeout: 8000 }).catch(() => undefined);
  check('history says how it is going so far', /0 of 4 runs started/.test((await page.getByTestId('campaign-summary').first().innerText().catch(() => '')) || ''));
  await api('POST', `/api/v1/flows/schedules/${camp.id}/delete`);

  // ---- notifications on the flows page ---------------------------------------------------------------------------
  await page.getByRole('button', { name: '← Flows' }).click();
  await page.getByTestId('notifications-toggle').waitFor();
  const unread = await page.getByTestId('unread-count').innerText({ timeout: 8000 });
  check('the bell shows how many are unread', Number(unread) >= 1, unread);
  await page.getByTestId('notifications-toggle').click();
  const list = await page.getByTestId('notifications').innerText();
  check('the run\'s outcome is listed', /e2e-schedule/.test(list) && /succeeded/.test(list), list);
  await page.getByRole('button', { name: /Mark all read/ }).click();
  check('marking read clears the count', await page.getByTestId('unread-count').waitFor({ state: 'detached', timeout: 8000 }).then(() => true, () => false));

  // ---- the dashboard, and deleting ---------------------------------------------------------------------------------
  const dash = page.getByTestId('schedule-dashboard');
  await dash.waitFor();
  check('the flows page lists every scheduled flow', /e2e-schedule/.test(await dash.innerText()));
  await dash.getByRole('button', { name: /Delete/ }).first().click();
  await until('schedule deleted', async () => (await schedules()).length === 0);
  check('deleting a schedule removes it', true);
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
