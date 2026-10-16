// Live test of a campaign on the real clock: a schedule that runs every minute and is limited to two runs must
// start exactly two runs, then switch itself off, say how it went, and stay off. Takes about three minutes.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. node e2e/flows-campaign.e2e.mjs
//
// It deletes its flow and schedule at the end.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const CORE = 'http://127.0.0.1:8767';
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
const until = async (what, fn, ms = 240000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 2000));
  }
  throw new Error(`timed out: ${what}`);
};

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));

try {
  const flow = (await api('POST', '/api/v1/flows', {
    name: 'e2e-campaign',
    variables: [{ name: 'topic', label: 'Topic', kind: 'text', required: true, default: 'none' }],
    nodes: [
      { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
      { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'topic is {{vars.topic}}' } },
    ],
    edges: [{ from: 'start', to: 'say' }],
  })).json;
  const made = await api('POST', `/api/v1/flows/${flow.id}/schedules`, { name: 'Two quick runs', when: { type: 'cron', cron: '* * * * *' }, timezone: 'UTC', misfire: 'skip', campaign: { max_runs: 2, series: [{ topic: 'first' }, { topic: 'second' }] } });
  check('a campaign that runs every minute for two runs is accepted', made.status === 201, JSON.stringify(made.json));
  const sid = made.json.schedule.id;
  const view = async () => (await api('GET', `/api/v1/flows/schedules/${sid}/campaign`)).json.campaign;
  check('before it starts it says two are to come', (await view()).total_planned === 2 && (await view()).remaining === 2, JSON.stringify(await view()));

  await until('both runs to be made and the schedule to switch itself off', async () => {
    const s = (await api('GET', `/api/v1/flows/schedules/${sid}`)).json.schedule;
    return s.runs_started >= 2 && !s.enabled;
  });
  const done = (await api('GET', `/api/v1/flows/schedules/${sid}`)).json.schedule;
  check('it started exactly two runs and switched itself off', done.runs_started === 2 && done.enabled === false && done.next_run_at === null, JSON.stringify(done));
  const runs = (await api('GET', `/api/v1/flows/${flow.id}/runs`)).json.runs.reverse();
  check('the runs took the inputs in turn', runs.length === 2 && runs[0].vars.topic === 'first' && runs[1].vars.topic === 'second', JSON.stringify(runs.map((r) => r.vars)));
  await until('the closing notice', async () => (await api('GET', '/api/v1/flows/notifications?limit=50')).json.notifications.some((n) => n.kind === 'campaign' && n.schedule_id === sid));
  const note = (await api('GET', '/api/v1/flows/notifications?limit=50')).json.notifications.find((n) => n.kind === 'campaign' && n.schedule_id === sid);
  check('it says the campaign has finished and how it went', /finished/.test(note.title) && /2 of 2 runs started/.test(note.body) && /2 worked/.test(note.body), `${note.title} | ${note.body}`);
  const list = (await api('GET', `/api/v1/flows/${flow.id}/schedules`)).json.schedules;
  check('the dashboard shows it as done, not paused', list[0].state === 'completed', list[0].state);
  const v = await view();
  check('the campaign view agrees', v.over === true && v.remaining === 0 && v.worked === 2, JSON.stringify(v));
  await new Promise((r) => setTimeout(r, 75000));
  const later = (await api('GET', `/api/v1/flows/${flow.id}/runs`)).json.runs;
  check('and no third run comes, however long it waits', later.length === 2, String(later.length));
  const off = await api('POST', `/api/v1/flows/schedules/${sid}/enable`);
  check('a finished campaign cannot be switched on again', off.status === 400 && /made all its runs/.test(off.json.error), JSON.stringify(off.json));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
