// Live rehearsal of the Facebook posting steps, in real Edge, against a Facebook-SHAPED stand-in page
// (e2e/standins/facebook-standin.html). It posts nothing to Facebook or anywhere else, and no Facebook account is used. It shows the
// recipe works against a page laid out the way the recipe expects; it does not prove that Facebook's own page looks like that.
// The first real post is the person's: see "Facebook" in documentation/AI_FLOWS.md section 11.
//
// This file was written without being run. Run it by hand, and never against the Aqua that is in use (port 8767):
//   1. serve the stand-in (from apps/aqua-desktop/e2e/standins):  python -m http.server 8792 --bind 127.0.0.1
//   2. start a SECOND core in a folder of its own (scripts/eval/core.py shows how: AQUA_WORKSPACE_ROOT=<folder>, AQUA_API_PORT=8790), with two more variables:
//        AQUA_PLATFORM_FACEBOOK=on
//        AQUA_SOCIAL_FACEBOOK_URL=http://127.0.0.1:8792/facebook-standin.html
//   3. from apps/aqua-desktop:  AQUA_E2E_ROOT=<that folder> AQUA_E2E_PORT=8790 node e2e/flows-facebook.e2e.mjs

import { readFileSync, existsSync, readdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '..', '..', '..');
const PORT = process.env.AQUA_E2E_PORT || '8790';
const ROOT = process.env.AQUA_E2E_ROOT ? path.resolve(process.env.AQUA_E2E_ROOT) : '';
if (PORT === '8767') {
  console.error(`refusing to run against port ${PORT}: that is the Aqua that is in use. Start a second core (see the top of this file) and give its port in AQUA_E2E_PORT.`);
  process.exit(2);
}
if (!ROOT || ROOT === repo || !existsSync(path.join(ROOT, 'data', '.aqua_ui_token'))) {
  console.error('AQUA_E2E_ROOT must be the folder of the second core (its data folder holds its own UI token). It must not be this repository.');
  process.exit(2);
}
const CORE = `http://127.0.0.1:${PORT}`;
const uiToken = readFileSync(path.join(ROOT, 'data', '.aqua_ui_token'), 'utf8').trim();
const uploads = path.join(ROOT, 'data', 'browser', 'uploads');
const STANDIN_POST = 'http://127.0.0.1:8792/facebook-standin.html?post=1';

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

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));
const uploadsBefore = new Set(existsSync(uploads) ? readdirSync(uploads) : []);

const flow = (name, text, approval = 'always', platform = 'facebook', action = 'publish') => ({
  name,
  settings: { approval },
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'cover', kind: 'image_generation', name: 'Draw the cover', config: { provider: 'card', title: 'Five ways agents save you time', style: 'ocean', size: '1200x627' } },
    { id: 'post', kind: 'social_media', name: `Post on ${platform}`, config: { platform, action, text, hashtags: 'ai agents', media: '{{nodes.cover.image}}' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'posted: {{nodes.post.post_url}}' } },
  ],
  edges: [{ from: 'start', to: 'cover' }, { from: 'cover', to: 'post' }, { from: 'post', to: 'say' }],
});

try {
  // ---- the switch is on, and the flow says what it will do -------------------------------------------------------------
  const good = (await api('POST', '/api/v1/flows', flow('e2e-facebook-good', 'Agents can save you real time.'))).json;
  const report = (await api('GET', `/api/v1/flows/${good.id}/validate`)).json;
  check('with the switch on, the flow is not called "not built", and Aqua says it will ask before posting', !report.issues.some((i) => i.severity === 'error' || i.code === 'platform_not_built') && report.needs_approval.includes('post') && report.issues.some((i) => i.code === 'will_ask'), JSON.stringify(report.issues.map((i) => i.code)));

  // ---- it asks first, then posts ---------------------------------------------------------------------------------------
  const r1 = (await api('POST', `/api/v1/flows/${good.id}/run`, {})).json.run.id;
  await until('the approval', async () => (await run(r1)).run.status === 'waiting');
  const w = await run(r1);
  const gate = w.nodes.find((n) => n.status === 'waiting');
  check('it drew the cover first, then stopped at the post and asked', gate?.node_id === 'post' && w.nodes.find((n) => n.node_id === 'cover').status === 'succeeded', JSON.stringify(w.nodes.map((n) => [n.node_id, n.status])));
  check('the approval shows the platform and the words', /facebook/.test(JSON.stringify(gate.wait)) && /Agents can save you real time/.test(JSON.stringify(gate.wait)), JSON.stringify(gate.wait));
  await api('POST', `/api/v1/flows/runs/${r1}/respond`, { node: 'post', decision: 'approved' });
  await until('the run to finish', async () => ['succeeded', 'failed'].includes((await run(r1)).run.status));
  const done = await run(r1);
  const post = done.nodes.find((n) => n.node_id === 'post');
  check('the post went through in real Edge', done.run.status === 'succeeded' && post.status === 'succeeded', `${done.run.status} ${done.run.error} ${post.error}`);
  check('the page showed the notice and the window had closed, so the status says so', /^published: /.test(post.outputs.status), JSON.stringify(post.outputs));
  check('the link to the post was read from the page and made whole', post.outputs.post_url === STANDIN_POST, post.outputs.post_url);
  const stage = readdirSync(uploads).filter((f) => !uploadsBefore.has(f));
  check('the image was copied to the uploads folder and attached from there', stage.some((f) => /^post_.*\.png$/.test(f)), stage.join(','));
  const events = (await api('GET', `/api/v1/flows/runs/${r1}/events?limit=500`)).json.events.map((e) => e.message);
  check('the words of the post are not copied into the run log', events.every((m) => !m.includes('Agents can save you real time')));
  check('the log says which audience the window showed (so the person can see it was "Only me")', events.some((m) => /The post window says: .*Only me/.test(m)), events.join(' | '));
  const verdict = (await api('GET', `/api/v1/flows/runs/${r1}/recovery`)).json.verdict;
  check('the health verdict counts it as a real success (there is proof)', verdict.kind === 'success', JSON.stringify(verdict));

  // ---- pressed, but the page never says it is shared: not success -----------------------------------------------------------
  const quiet = (await api('POST', '/api/v1/flows', flow('e2e-facebook-silent', 'This one never confirms [fail-silently]', 'never'))).json.id;
  const r2 = (await api('POST', `/api/v1/flows/${quiet}/run`, {})).json.run.id;
  await until('the step to stop', async () => (await run(r2)).nodes.some((n) => n.node_id === 'post' && n.status === 'needs_action'));
  const q = await run(r2);
  const qp = q.nodes.find((n) => n.node_id === 'post');
  check('with no confirmation the step stops for a person instead of claiming success', qp.status === 'needs_action' && /not known whether it was published/.test(qp.error) && !qp.outputs.post_url, JSON.stringify(qp));
  await until('the run to wait', async () => (await run(r2)).run.status === 'waiting');
  const shown = (await api('GET', '/api/v1/flows/inbox')).json.items.find((i) => i.run_id === r2);
  check('it is in the inbox with the choices, including "it was done"', shown && JSON.stringify(shown.wait).includes('done'), JSON.stringify(shown));
  await api('POST', `/api/v1/flows/runs/${r2}/stop`);

  // ---- what the recipe does not do, and the other platforms, are said plainly ------------------------------------------------
  const comments = (await api('POST', '/api/v1/flows', flow('e2e-facebook-comments', 'hi', 'never', 'facebook', 'read_comments'))).json.id;
  const vc = (await api('GET', `/api/v1/flows/${comments}/validate`)).json;
  check('reading comments on Facebook is flagged as not supported by the recipe', vc.issues.some((x) => x.code === 'unsupported_action'), JSON.stringify(vc.issues.map((x) => x.code)));
  const insta = (await api('POST', '/api/v1/flows', flow('e2e-facebook-insta', 'hi', 'never', 'instagram'))).json.id;
  const vi = (await api('GET', `/api/v1/flows/${insta}/validate`)).json;
  check('another platform is still flagged as not built when the flow is checked', vi.issues.some((x) => x.code === 'platform_not_built'), JSON.stringify(vi.issues.map((x) => x.code)));
  const r3 = await api('POST', `/api/v1/flows/${insta}/run`, {});
  check('and a real run of it is refused before anything starts, naming what can be posted to', r3.status >= 400 && /not built yet/.test(JSON.stringify(r3.json)) && /LinkedIn and Facebook/.test(JSON.stringify(r3.json)), JSON.stringify(r3));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  if (existsSync(uploads)) for (const f of readdirSync(uploads)) if (!uploadsBefore.has(f)) rmSync(path.join(uploads, f), { force: true });
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
