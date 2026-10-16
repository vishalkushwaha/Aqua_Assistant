// Browser test for workflow health, the error dashboard and asking about workflows, against the real UI and
// the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows-health.e2e.mjs
//
// One flow reads a file that does not exist (it really fails, twice); the other only sends an in-app
// notice (it really works, twice). The test deletes both at the end and touches nothing else.

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

// What Aqua has learned about fixes lives in the real database; start these checks from a clean slate.
await api('POST', '/api/v1/flows/recovery/knowledge/reset', {});

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));

const broken = {
  name: 'e2e-health-broken',
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'read', kind: 'file', name: 'Read the report', config: { operation: 'read', path: 'flow_e2e_missing/nothing-here.txt' } },
  ],
  edges: [{ from: 'start', to: 'read' }],
};
const fine = {
  name: 'e2e-health-fine',
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'hello' } },
  ],
  edges: [{ from: 'start', to: 'say' }],
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1200 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  const b = (await api('POST', '/api/v1/flows', broken)).json.id;
  const f = (await api('POST', '/api/v1/flows', fine)).json.id;
  for (const id of [b, b, f, f]) {
    const r = await api('POST', `/api/v1/flows/${id}/run`, {});
    await until('the run to finish', async () => ['failed', 'succeeded'].includes((await api('GET', `/api/v1/flows/runs/${r.json.run.id}`)).json.run.status));
  }

  // ---- the API, before the browser -------------------------------------------------------------------------------------
  const health = (await api('GET', '/api/v1/flows/health')).json;
  const hb = health.flows.find((x) => x.health.flow_id === b);
  const hf = health.flows.find((x) => x.health.flow_id === f);
  check('a flow that failed twice is failing, one that worked twice is healthy', hb.health.state === 'failing' && hb.health.score === 0 && hf.health.state === 'healthy' && hf.health.score === 100, JSON.stringify([hb.health.state, hf.health.state]));
  check('the history has fourteen days and today holds the runs', hb.history.length === 14 && hb.history[13].failed === 2 && hf.history[13].success === 2);
  check('failures are grouped by kind', health.errors.some((g) => g.label === 'Missing file' && g.count >= 2));

  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByTestId('health-dashboard').waitFor();

  // ---- the dashboard ---------------------------------------------------------------------------------------------------------
  const card = (name) => page.getByTestId('health-card').filter({ hasText: name });
  await card('e2e-health-broken').waitFor();
  check('the failing flow shows as failing with its score', /failing/.test(await card('e2e-health-broken').innerText()) && /· 0/.test(await card('e2e-health-broken').innerText()));
  check('the healthy flow shows as healthy', /healthy · 100/.test(await card('e2e-health-fine').innerText()));
  check('each card draws the last fourteen days', (await card('e2e-health-broken').getByTestId('history').locator('> div').count()) === 14);
  check('the card says why', /last 2 runs in a row failed/i.test(await card('e2e-health-broken').innerText()) && /Missing file/.test(await card('e2e-health-broken').innerText()), await card('e2e-health-broken').innerText());
  check('the error dashboard lists the kind of failure', /Missing file/.test(await page.getByTestId('error-dashboard').innerText()));

  // ---- asking ------------------------------------------------------------------------------------------------------------------
  const ask = async (q) => {
    const prev = (await page.getByTestId('status-reply').innerText().catch(() => '')) || '';
    await page.getByLabel('Ask about your workflows').fill(q);
    await page.getByRole('button', { name: 'Ask', exact: true }).click();
    await page.getByTestId('status-reply').waitFor();
    await until('a new answer', async () => (await page.getByTestId('status-reply').innerText()) !== prev);
    return page.getByTestId('status-reply');
  };
  let reply = await ask('How are my workflows doing?');
  check('a status question is answered from the record', /e2e-health-broken: failing/.test(await reply.innerText()) && /e2e-health-fine: healthy/.test(await reply.innerText()), await reply.innerText());
  reply = await ask('Fix the last error');
  const text = await reply.innerText();
  check('"fix the last error" names the step and the kind of problem', /Read the report/.test(text) && /Missing file/.test(text), text);
  check('it says nothing is changed until approval', /nothing is changed until then/.test(text));
  const v0 = (await api('GET', `/api/v1/flows/${b}`)).json.version;
  await reply.getByRole('button', { name: 'Approve and apply' }).first().waitFor();
  check('and the flow is untouched so far', v0 === 1);
  await reply.getByRole('button', { name: 'Approve and apply' }).first().click();
  await page.getByTestId('status-note').waitFor();
  const after = (await api('GET', `/api/v1/flows/${b}`)).json;
  check('approving from the chat applies it as a new version', after.version === v0 + 1 && after.nodes.find((n) => n.id === 'read').on_error === 'continue');
  reply = await ask('Check my workflows and fix any problems you can');
  const all = await reply.innerText();
  check('"check my workflows" lists the problem and what only the person can do', /e2e-health-broken/.test(all) && /Check that the file or folder exists/.test(all), all);
  check('and does not list the healthy one as a problem', !/e2e-health-fine:/.test(all));
  reply = await ask('write me a poem');
  check('anything else gets a pointer to what can be asked', /Did my scheduled workflow run/.test(await reply.innerText()));
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  await api('POST', '/api/v1/flows/recovery/knowledge/reset', {}).catch(() => undefined);
  for (const x of await created().catch(() => [])) await api('POST', `/api/v1/flows/${x.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
