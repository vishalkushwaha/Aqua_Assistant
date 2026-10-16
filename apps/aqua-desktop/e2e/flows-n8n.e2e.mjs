// Live rehearsal of the n8n step against a STAND-IN for n8n (a tiny local server this script runs). No real n8n is
// used, so this shows Aqua's side works (approval, the call, the answer, health, failures); it does not prove a
// particular n8n workflow does what you built it to do.
//
//   1. start aqua-core with AQUA_N8N_URL=http://127.0.0.1:5699   (target\debug\aqua-core.exe, repo root)
//   2. start the UI               (npm run dev, port 1420)
//   3. node e2e/flows-n8n.e2e.mjs
//
// It deletes its flows at the end.

import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');

const UI = process.env.AQUA_UI_URL ?? 'http://127.0.0.1:1420';
const CORE = 'http://127.0.0.1:8767';
const N8N_PORT = 5699;
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};
const api = async (method, url, body) => {
  const r = await fetch(`${CORE}${url}`, { method, headers: { Authorization: `Bearer ${uiToken}`, 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null) };
};
const until = async (what, fn, ms = 30000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out: ${what}`);
};
const run = (id) => api('GET', `/api/v1/flows/runs/${id}`).then((r) => r.json);

// ---- the stand-in n8n ----------------------------------------------------------------------------------------------------------
const seen = [];
const server = createServer((req, res) => {
  let body = '';
  req.on('data', (c) => (body += c));
  req.on('end', () => {
    seen.push({ method: req.method, url: req.url, headers: req.headers, body });
    if (req.url === '/healthz') return res.writeHead(200, { 'Content-Type': 'application/json' }).end('{"status":"ok"}');
    if (req.url === '/webhook/add-row') return res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ rows_added: 1, echo: JSON.parse(body || '{}') }));
    res.writeHead(404, { 'Content-Type': 'application/json' }).end('{"message":"not registered"}');
  });
});
await new Promise((r) => server.listen(N8N_PORT, '127.0.0.1', r));

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));
const flow = (name, workflow, approval) => ({
  name,
  settings: { approval },
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', position: { x: 60, y: 40 }, config: { trigger_type: 'manual' } },
    { id: 'call', kind: 'n8n', position: { x: 60, y: 200 }, name: 'Add to the sheet', config: { workflow, payload: { who: 'Ada', n: 3 } } },
    { id: 'say', kind: 'notification', name: 'Tell me', position: { x: 60, y: 360 }, config: { channel: 'in_app', message: 'n8n said {{nodes.call.status}} and added {{nodes.call.result.rows_added}}' } },
  ],
  edges: [{ from: 'start', to: 'call' }, { from: 'call', to: 'say' }],
});

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1100 } });
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  const h = (await api('GET', '/api/v1/flows/n8n/health')).json;
  check('the health check finds the stand-in n8n', h.configured && h.reachable && h.base_url === `http://127.0.0.1:${N8N_PORT}`, JSON.stringify(h));
  check('it asked only n8n\'s health address', seen.length === 1 && seen[0].url === '/healthz', JSON.stringify(seen.map((s) => s.url)));

  // ---- it asks first, then calls once ---------------------------------------------------------------------------------------
  const good = (await api('POST', '/api/v1/flows', flow('e2e-n8n-good', 'add-row', 'always'))).json;
  const report = (await api('GET', `/api/v1/flows/${good.id}/validate`)).json;
  check('the flow validates and says Aqua will ask before calling n8n', !report.issues.some((i) => i.level === 'error') && report.needs_approval.includes('call'), JSON.stringify(report));
  const startedAt = seen.length;
  const r1 = (await api('POST', `/api/v1/flows/${good.id}/run`, {})).json.run.id;
  await until('the approval', async () => (await run(r1)).run.status === 'waiting');
  check('it stopped before calling n8n and nothing was sent', seen.length === startedAt && (await run(r1)).nodes.find((n) => n.node_id === 'call').status === 'waiting', JSON.stringify(seen.slice(startedAt).map((s) => s.url)));
  await api('POST', `/api/v1/flows/runs/${r1}/respond`, { node: 'call', decision: 'approved' });
  await until('the run to finish', async () => ['succeeded', 'failed'].includes((await run(r1)).run.status));
  const done = await run(r1);
  check('after approval the run finished', done.run.status === 'succeeded', `${done.run.error} ${done.nodes.map((n) => n.error).join(' ')}`);
  const calls = seen.slice(startedAt);
  check('n8n was called exactly once, by webhook, with the data as JSON', calls.length === 1 && calls[0].method === 'POST' && calls[0].url === '/webhook/add-row' && JSON.parse(calls[0].body).who === 'Ada' && JSON.parse(calls[0].body).n === 3, JSON.stringify(calls));
  check('with an idempotency key so n8n can recognise a repeat', calls[0].headers['idempotency-key'] === `aqua-run${r1}-call`, calls[0].headers['idempotency-key']);
  const events = (await api('GET', `/api/v1/flows/runs/${r1}/events?limit=200`)).json.events.map((e) => e.message);
  check('n8n\'s answer reached the next step', events.some((m) => m.includes('n8n said 200 and added 1')), events.join(' | '));
  check('the run counts as a real success, with the answer as proof', (await api('GET', `/api/v1/flows/runs/${r1}/recovery`)).json.verdict.kind === 'success');

  // ---- a workflow that does not exist ------------------------------------------------------------------------------------------
  const missing = (await api('POST', '/api/v1/flows', flow('e2e-n8n-missing', 'not-there', 'never'))).json;
  const r2 = (await api('POST', `/api/v1/flows/${missing.id}/run`, {})).json.run.id;
  await until('the failure', async () => (await run(r2)).run.status === 'failed');
  const err = (await run(r2)).nodes.find((n) => n.node_id === 'call').error;
  check('a webhook n8n does not know says how to fix it', /was not found/.test(err) && /active/.test(err), err);

  // ---- the step in the editor shows n8n's state -----------------------------------------------------------------------------------
  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-n8n-good').first().waitFor();
  await page.locator('div', { has: page.getByText('e2e-n8n-good', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Open/ }) }).last().getByRole('button', { name: /Open/ }).click();
  await page.locator('[data-node="call"]').click();
  await page.getByTestId('n8n-status').waitFor();
  check('the step\'s form says n8n is up', /n8n on this computer: up/.test(await page.getByTestId('n8n-status').innerText()), await page.getByTestId('n8n-status').innerText());
  check('and offers the webhook path, the data and the credential', (await page.getByLabel('Credential', { exact: true }).count()) === 1 && /Webhook path/.test(await page.getByTestId('side-panel').innerText()));

  // ---- n8n goes away --------------------------------------------------------------------------------------------------------------
  await new Promise((r) => server.close(r));
  server.closeAllConnections?.();
  const down = (await api('GET', '/api/v1/flows/n8n/health')).json;
  check('with n8n stopped, health says it is not answering', down.configured && !down.reachable && /is it running/.test(down.detail), JSON.stringify(down));
  await page.getByTestId('n8n-status').getByRole('button', { name: 'Check again' }).click();
  await until('the form to notice', async () => /not answering/.test(await page.getByTestId('n8n-status').innerText()));
  check('the editor notices too', true);
  const r3 = (await api('POST', `/api/v1/flows/${missing.id}/run`, {})).json.run.id;
  await until('the failure', async () => (await run(r3)).run.status === 'failed');
  const e3 = (await run(r3)).nodes.find((n) => n.node_id === 'call').error;
  check('a run while n8n is down fails as a connection problem, not an unknown outcome', /could not connect to n8n/.test(e3), e3);
  const rec = (await api('GET', `/api/v1/flows/runs/${r3}/recovery`)).json;
  check('and it is classed as a network problem that can be retried', rec.steps[0].class === 'network', JSON.stringify(rec.steps[0].class));
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  server.close();
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
