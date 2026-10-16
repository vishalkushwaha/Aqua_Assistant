// Live test of the browser steps: a real flow drives real Edge (Aqua's managed browser) against the local
// fixture site, through the real guarded backend.
//
//   1. serve core/tests/fixtures/browser_site on 127.0.0.1:8791   (python -m http.server 8791 --bind 127.0.0.1)
//   2. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   3. node e2e/flows-browser.e2e.mjs
//
// It fills the fixture contact form and clicks Send. The click is approved through the flow (the step's own
// approval), which is what lets an unattended run press it. It deletes its flows and touches nothing else.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const CORE = 'http://127.0.0.1:8767';
const SITE = 'http://127.0.0.1:8791';
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
const until = async (what, fn, ms = 90000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`timed out: ${what}`);
};
const run = (id) => api('GET', `/api/v1/flows/runs/${id}`).then((r) => r.json);
const events = async (id) => (await api('GET', `/api/v1/flows/runs/${id}/events?limit=500`)).json.events.map((e) => e.message);

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));

const b = (id, name, config) => ({ id, kind: 'browser', name, config });
const fillAndSend = (name, approval) => ({
  name,
  settings: { approval },
  variables: [{ name: 'who', label: 'Name', kind: 'text', required: true, default: 'Ada' }],
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'open', kind: 'website', name: 'Open the form', config: { url: `${SITE}/form.html`, require_login: true } },
    b('name', 'Type the name', { action: 'type', target: 'textbox:Name', value: '{{vars.who}}' }),
    b('email', 'Type the email', { action: 'type', target: 'textbox:Email', value: 'ada@example.com' }),
    b('topic', 'Choose the topic', { action: 'select', target: 'combobox:Topic', value: 'Support' }),
    b('send', 'Press Send', { action: 'click', target: 'button:Send' }),
    b('wait', 'Wait for the confirmation', { action: 'wait_for', value: 'Message sent to' }),
    b('shot', 'Take a screenshot', { action: 'screenshot' }),
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'opened: {{nodes.open.page}} / {{nodes.send.result}} / shot: {{nodes.shot.result}}' } },
  ],
  edges: [['start', 'open'], ['open', 'name'], ['name', 'email'], ['email', 'topic'], ['topic', 'send'], ['send', 'wait'], ['wait', 'shot'], ['shot', 'say']].map(([from, to]) => ({ from, to })),
});

try {
  // ---- with the flow's own approval: it waits before the click, then the click is let through ----------------------------
  const mk = await api('POST', '/api/v1/flows', fillAndSend('e2e-browser-ask', 'always')); if (!mk.json?.id) throw new Error('create failed: ' + JSON.stringify(mk.json)); const asked = mk.json.id;
  const s1 = await api('POST', `/api/v1/flows/${asked}/run`, { inputs: { who: 'Grace' } }); if (!s1.json?.run) throw new Error('start failed: ' + JSON.stringify(s1.json)); const r1 = s1.json.run.id;
  await until('the click to ask', async () => (await run(r1)).run.status === 'waiting');
  const w = await run(r1);
  const gate = w.nodes.find((n) => n.status === 'waiting');
  check('the run stops before the click and asks, having filled the form already', gate?.node_id === 'send' && w.nodes.find((n) => n.node_id === 'name').status === 'succeeded' && w.nodes.find((n) => n.node_id === 'topic').status === 'succeeded', JSON.stringify(w.nodes.map((n) => [n.node_id, n.status])));
  check('the open step saw no sign-in form', w.nodes.find((n) => n.node_id === 'open').outputs.logged_in === true);
  check('the approval says what it will click', /button:Send/.test(JSON.stringify(gate.wait)), JSON.stringify(gate.wait));
  const ok = await api('POST', `/api/v1/flows/runs/${r1}/respond`, { node: 'send', decision: 'approved' });
  check('approving is accepted', ok.status === 200, JSON.stringify(ok.json));
  await until('the run to finish', async () => ['succeeded', 'failed'].includes((await run(r1)).run.status));
  const done = await run(r1);
  const log1 = await events(r1);
  check('the whole flow worked in the real browser', done.run.status === 'succeeded', `${done.run.status}: ${done.run.error} ${JSON.stringify(done.nodes.filter((n) => n.error).map((n) => [n.node_id, n.error]))}`);
  check('the page itself confirmed it, with the name typed from the flow input', log1.some((m) => /wait for the confirmation/i.test(m)) && done.nodes.find((n) => n.node_id === 'wait').status === 'succeeded');
  check('a screenshot was saved by the browser', /data[\\/]browser[\\/]artifacts/.test(String(done.nodes.find((n) => n.node_id === 'shot').outputs.result)), String(done.nodes.find((n) => n.node_id === 'shot').outputs.result));
  check('what was typed is not in the run log', log1.every((m) => !m.includes('Grace') && !m.includes('ada@example.com')), log1.join(' | '));

  // ---- the browser's own audit log records the approved click ------------------------------------------------------------------
  const audit = readFileSync(path.join(root, 'data', 'browser', 'audit.jsonl'), 'utf8').trim().split('\n').slice(-40).map((l) => JSON.parse(l));
  const click = audit.filter((a) => a.tool === 'browser_click').pop();
  check('the audit log shows the click was allowed', click && click.ok === true, JSON.stringify(click));

  // ---- a page that changed: the error says so, in words, and the run does not click anything else --------------------------------
  const changed = fillAndSend('e2e-browser-changed', 'never');
  changed.nodes.find((n) => n.id === 'send').config.target = 'button:Submit message';
  const c = (await api('POST', '/api/v1/flows', changed)).json.id;
  const r2 = (await api('POST', `/api/v1/flows/${c}/run`, {})).json.run.id;
  await until('the run to fail', async () => (await run(r2)).run.status === 'failed');
  const failed = await run(r2);
  const err = failed.nodes.find((n) => n.node_id === 'send').error;
  check('a missing button is reported with what the page shows instead', /element not found/.test(err) && /Send/.test(err), err);
  const rec = (await api('GET', `/api/v1/flows/runs/${r2}/recovery`)).json;
  check('and it is recognised as the page having changed', rec.steps[0].class === 'website_ui_change' && /looks|page changed|Open the step/i.test(rec.steps[0].guidance ?? ''), JSON.stringify(rec.steps[0]));

  const fix = rec.steps[0].suggestions.find((x) => x.key === 'use_similar_element');
  check('the only button left on the page is offered as the fix, needing approval', fix && /button:Send/.test(fix.title) && fix.needs_approval && fix.changes_logic, JSON.stringify(rec.steps[0].suggestions.map((x) => x.key)));
  const applied = await api('POST', `/api/v1/flows/runs/${r2}/recovery/apply`, { node: 'send', key: 'use_similar_element' });
  check('approving it changes the step to point at that button', applied.status === 200 && (await api('GET', `/api/v1/flows/${c}`)).json.nodes.find((n) => n.id === 'send').config.target === 'button:Send', JSON.stringify(applied.json));
  const r2b = (await api('POST', `/api/v1/flows/${c}/run`, {})).json.run.id;
  await until('the repaired run to finish', async () => ['succeeded', 'failed'].includes((await run(r2b)).run.status));
  check('and the repaired flow now works in the real browser', (await run(r2b)).run.status === 'succeeded', JSON.stringify((await run(r2b)).nodes.filter((n) => n.error).map((n) => n.error)));

  // ---- a site the policy does not allow is refused, not visited ---------------------------------------------------------------
  const blocked = { name: 'e2e-browser-blocked', nodes: [{ id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } }, { id: 'open', kind: 'website', name: 'Open', config: { url: 'http://127.0.0.1:8767/api/v1/flows' } }], edges: [{ from: 'start', to: 'open' }] };
  const d = (await api('POST', '/api/v1/flows', blocked)).json.id;
  const r3 = (await api('POST', `/api/v1/flows/${d}/run`, {})).json.run.id;
  await until('the blocked run to fail', async () => (await run(r3)).run.status === 'failed');
  const bl = await run(r3);
  check("Aqua's own API is off limits to a flow's browser", /blocked by the browser policy|not allowed/i.test(bl.nodes.find((n) => n.node_id === 'open').error), bl.nodes.find((n) => n.node_id === 'open').error);
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
