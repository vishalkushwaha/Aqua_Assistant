// Browser test for the supervisor's outcome card and recovery, against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows-recovery.e2e.mjs
//
// The flow reads a file that does not exist, so the run really fails through the guarded tool host. The
// test approves a proposed fix in the browser, undoes it, and deletes the flow at the end.

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

const flowDef = {
  name: 'e2e-recovery',
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'read', kind: 'file', name: 'Read the report', config: { operation: 'read', path: 'flow_e2e_missing/nothing-here.txt' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'read it' } },
  ],
  edges: [{ from: 'start', to: 'read' }, { from: 'read', to: 'say' }],
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
  const flow = async () => (await api('GET', `/api/v1/flows/${flowId}`)).json;
  const started = await api('POST', `/api/v1/flows/${flowId}/run`, {});
  const runId = started.json.run.id;
  await until('the run to fail', async () => (await api('GET', `/api/v1/flows/runs/${runId}`)).json.run.status === 'failed');

  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-recovery').first().waitFor();
  await page.locator('div', { has: page.getByText('e2e-recovery', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Open/ }) }).last().getByRole('button', { name: /Open/ }).click();
  await page.locator('[data-node="read"]').waitFor();
  await page.getByRole('tab', { name: /^Run/ }).click();
  await page.getByText(/Earlier runs/).waitFor();
  await page.getByText(new RegExp(`#${runId}`)).first().click();

  const card = page.getByTestId('recovery-card');
  await card.waitFor();
  const text = await card.innerText();
  check('the outcome says it did not work and why', /Did not work/.test(text) && /Read the report/.test(text), text);
  check('the kind of problem is named in plain words', /Missing file/.test(text) && /file or folder/i.test(text), text);
  check('a problem only the person can fix gets guidance, not a retry', /Check that the file or folder exists/.test(text) && !(await page.getByTestId('fix-more_retries').count()), text);
  check('a change of logic is labelled as one', /changes what the flow does when a step fails/.test(text), text);
  check('the offer says it has not been tried before', /Not tried before/.test(text));

  const before = (await flow()).version;
  await page.getByTestId('fix-carry_on').getByRole('button', { name: 'Approve and apply' }).click();
  await page.getByTestId('fix-applied').waitFor();
  const after = await flow();
  check('approving applies it as a new version', after.version === before + 1 && after.nodes.find((n) => n.id === 'read').on_error === 'continue', `${before} -> ${after.version}`);
  check('the audit trail names the supervisor', (await api('GET', `/api/v1/flows/${flowId}/audit`)).json.audit.some((a) => a.actor === 'supervisor'));
  const recs = (await api('GET', `/api/v1/flows/${flowId}/recoveries`)).json.recoveries;
  check('the fix is recorded and its result is still unknown', recs.length === 1 && recs[0].outcome === 'pending' && recs[0].kind === 'permanent');

  await page.getByTestId('fix-applied').getByRole('button', { name: 'Undo' }).click();
  await until('undone', async () => (await flow()).nodes.find((n) => n.id === 'read').on_error === 'fail');
  check('undoing puts the step back as it was', true);
  check('and the undo is itself a version', (await flow()).version === before + 2);
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  await api('POST', '/api/v1/flows/recovery/knowledge/reset', {}).catch(() => undefined);
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`
${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
