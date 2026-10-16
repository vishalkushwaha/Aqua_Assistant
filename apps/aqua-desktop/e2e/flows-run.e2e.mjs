// Browser test for running flows, against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows-run.e2e.mjs
//
// The flow it runs uses real steps through the guarded tool host: it writes a small file in Aqua's
// workspace, asks a question, reads the file back and asks for approval. Every UI action is confirmed
// through the API. It deletes the flows it created and the file it wrote, and touches nothing else.

import { createRequire } from 'node:module';
import { readFileSync, existsSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');

const UI = process.env.AQUA_UI_URL ?? 'http://127.0.0.1:1420';
const CORE = 'http://127.0.0.1:8767';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();
const workspaceDir = path.join(os.homedir(), 'AquaWorkspace', 'flow_e2e');

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

const node = (id, kind, name, config) => ({ id, kind, name, config });
const flowDef = {
  name: 'e2e-run',
  variables: [{ name: 'label', label: 'File label', kind: 'text', required: true, default: null }],
  nodes: [
    node('start', 'trigger', 'Start', { trigger_type: 'manual' }),
    node('save', 'file', 'Write the file', { operation: 'write', path: 'flow_e2e/{{vars.label}}.txt', content: 'hello {{vars.label}}' }),
    node('ask', 'user_input', 'Keep it?', { question: 'Keep this file?', answer_kind: 'yes_no' }),
    node('keep', 'condition', 'Keep?', { expression: '{{nodes.ask.answer}}' }),
    node('read', 'file', 'Read it back', { operation: 'read', path: 'flow_e2e/{{vars.label}}.txt' }),
    node('ok', 'approval', 'Show the text', { title: 'Show this text?', preview: '{{nodes.read.content}}', allow_edit: true }),
    node('say', 'notification', 'Tell me', { channel: 'in_app', message: 'Final text: {{nodes.ok.text}}' }),
    node('bye', 'notification', 'Not kept', { channel: 'in_app', message: 'the file was not kept' }),
  ],
  edges: [
    { from: 'start', to: 'save' }, { from: 'save', to: 'ask' }, { from: 'ask', to: 'keep' },
    { from: 'keep', to: 'read', port: 'true' }, { from: 'keep', to: 'bye', port: 'false' },
    { from: 'read', to: 'ok' }, { from: 'ok', to: 'say', port: 'approved' },
  ],
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1100 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  rmSync(workspaceDir, { recursive: true, force: true });
  const made = await api('POST', '/api/v1/flows', flowDef);
  if (made.status !== 201) throw new Error(`could not create the flow: ${JSON.stringify(made.json)}`);
  const flowId = made.json.id;
  const runs = async () => (await api('GET', `/api/v1/flows/${flowId}/runs`)).json.runs;

  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-run').first().waitFor();
  await page.locator('div', { has: page.getByText('e2e-run', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Open/ }) }).last().getByRole('button', { name: /Open/ }).click();
  await page.locator('[data-node="save"]').waitFor();

  // ---- starting: the flow's input is asked for ---------------------------------------------------------------
  await page.getByTestId('run-button').click();
  await page.getByTestId('run-dialog').waitFor();
  check('running asks for the flow\'s inputs', await page.getByLabel('File label').isVisible());
  await page.getByRole('button', { name: /^Start$/ }).click();
  check('a missing required input is refused with its name', /missing: File label/.test(await page.getByRole('alert').textContent()));
  await page.getByLabel('File label').fill('one');
  await page.getByRole('button', { name: /^Start$/ }).click();
  await page.getByTestId('run-panel').waitFor();
  const run = await until('a run', async () => (await runs())[0]);

  // ---- live progress: the file was written by the real tool, then it stops and asks --------------------------------
  const ask = await page.getByTestId('wait-card').filter({ hasText: 'Keep this file?' }).waitFor().then(() => true);
  check('the run stops at the question and shows it', ask);
  const file = path.join(workspaceDir, 'one.txt');
  check('the file step really ran through the tool host', existsSync(file) && readFileSync(file, 'utf8') === 'hello one', file);
  check('the canvas shows each step\'s state', (await page.locator('[data-run-status="succeeded"]').count()) >= 2 && (await page.locator('[data-run-status="waiting"]').count()) === 1);
  const asked = (await api('GET', `/api/v1/flows/runs/${run.id}`)).json;
  check('nothing is running while it waits for you', asked.run.status === 'waiting' && asked.nodes.find((n) => n.node_id === 'ask').status === 'waiting');
  await page.getByTestId('wait-card').getByRole('button', { name: 'No', exact: true }).waitFor();

  // ---- answer it: the untaken branch is skipped, the other runs -----------------------------------------------------------
  await page.getByTestId('wait-card').getByRole('button', { name: 'Yes', exact: true }).click();
  await page.getByTestId('wait-card').filter({ hasText: 'Show this text?' }).waitFor();
  const preview = await page.getByLabel('Preview (you can edit it)').inputValue();
  check('the approval shows what the earlier steps produced', preview === 'hello one', preview);
  const mid = (await api('GET', `/api/v1/flows/runs/${run.id}`)).json;
  check('the branch that was not taken is skipped', mid.nodes.find((n) => n.node_id === 'bye').status === 'skipped');
  await page.getByLabel('Preview (you can edit it)').fill('hello one, edited');
  await page.getByTestId('wait-card').getByRole('button', { name: 'Approve', exact: true }).click();
  await until('the run to finish', async () => (await api('GET', `/api/v1/flows/runs/${run.id}`)).json.run.status === 'succeeded');
  check('approving with an edit passes the edit on', await page.getByTestId('run-log').getByText('Final text: hello one, edited').waitFor({ timeout: 8000 }).then(() => true, () => false));
  check('the run is shown as done', await page.getByTestId('run-panel').getByText('succeeded', { exact: true }).first().waitFor({ timeout: 8000 }).then(() => true, () => false));
  await page.getByTestId('run-panel').locator('[data-step="say"]').click();
  check('clicking a step shows what it produced', await page.getByTestId('step-detail').isVisible());
  await page.getByTestId('run-panel').locator('[data-step="read"]').click();
  check('and its outputs', (await page.getByTestId('step-detail').innerText()).includes('hello one'));

  // ---- the log can be filtered and searched ------------------------------------------------------------------------------
  await page.getByLabel('Log level').selectOption('notify');
  const onlyMessages = await page.getByTestId('run-log').innerText();
  check('the log can be filtered to messages', onlyMessages.includes('Final text') && !onlyMessages.includes('Started'), onlyMessages);
  await page.getByLabel('Log level').selectOption('all');
  await page.getByLabel('Search the log').fill('approved');
  check('and searched', (await page.getByTestId('run-log').innerText()).toLowerCase().includes('approved') && !(await page.getByTestId('run-log').innerText()).includes('Started'));
  await page.getByLabel('Search the log').fill('');

  // ---- the other answer: a different path -----------------------------------------------------------------------------------
  await page.getByRole('button', { name: 'Close the run' }).click();
  check('earlier runs are listed', await page.getByText('Earlier runs').waitFor({ timeout: 8000 }).then(() => true, () => false));
  await page.getByRole('button', { name: /Run this flow/ }).click();
  await page.getByLabel('File label').fill('two');
  await page.getByRole('button', { name: /^Start$/ }).click();
  await page.getByTestId('wait-card').filter({ hasText: 'Keep this file?' }).waitFor();
  await page.getByTestId('wait-card').getByRole('button', { name: 'No', exact: true }).click();
  await until('second run to finish', async () => (await runs())[0].status === 'succeeded');
  const two = (await runs())[0];
  const second = (await api('GET', `/api/v1/flows/runs/${two.id}`)).json;
  check('answering no takes the other path and skips the rest', second.nodes.find((n) => n.node_id === 'bye').status === 'succeeded' && second.nodes.find((n) => n.node_id === 'read').status === 'skipped' && second.nodes.find((n) => n.node_id === 'ok').status === 'skipped');

  // ---- stopping a waiting run, and the inbox --------------------------------------------------------------------------------
  await page.getByRole('button', { name: 'Close the run' }).click();
  await page.getByRole('button', { name: /Run this flow/ }).click();
  await page.getByLabel('File label').fill('three');
  await page.getByRole('button', { name: /^Start$/ }).click();
  await page.getByTestId('wait-card').filter({ hasText: 'Keep this file?' }).waitFor();
  await page.getByRole('button', { name: '← Flows' }).click();
  await page.getByTestId('inbox').waitFor();
  check('a waiting run appears in the inbox on the flows page', /Waiting for you/.test(await page.getByTestId('inbox').innerText()) && /Keep this file\?/.test(await page.getByTestId('inbox').innerText()));
  await page.getByTestId('inbox').getByRole('button', { name: 'Open' }).first().click();
  await page.getByTestId('wait-card').filter({ hasText: 'Keep this file?' }).waitFor();
  check('opening it goes straight to the question', true);
  await page.getByTestId('run-panel').getByRole('button', { name: 'Stop', exact: true }).click();
  const stopped = await until('cancelled', async () => (await runs())[0].status === 'cancelled' && (await runs())[0]);
  check('stopping a waiting run cancels it', stopped.status === 'cancelled');

  // ---- a dry run ----------------------------------------------------------------------------------------------------------------
  await page.getByRole('button', { name: /Run this flow/ }).click().catch(() => {});
  if (!(await page.getByTestId('run-dialog').isVisible().catch(() => false))) {
    await page.getByRole('button', { name: 'Close the run' }).click();
    await page.getByRole('button', { name: /Run this flow/ }).click();
  }
  await page.getByLabel('File label').fill('dry');
  await page.getByLabel('Dry run').check();
  await page.getByRole('button', { name: /Start dry run/ }).click();
  await until('dry run finished', async () => (await runs())[0].dry_run && (await runs())[0].status === 'succeeded');
  check('a dry run finishes without asking and without touching the file system', !existsSync(path.join(workspaceDir, 'dry.txt')));
  check('and says it was a dry run', await page.getByTestId('run-log').getByText(/Would run/).first().waitFor({ timeout: 8000 }).then(() => true, () => false), await page.getByTestId('run-log').innerText());
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  rmSync(workspaceDir, { recursive: true, force: true });
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
