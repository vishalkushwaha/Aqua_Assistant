// Live rehearsal of the LinkedIn posting steps, in real Edge, against a LinkedIn-SHAPED fixture page
// (core/tests/fixtures/browser_site/feed.html). It posts nothing to LinkedIn or anywhere else, and no LinkedIn
// account is used. It shows the recipe works against a page laid out that way; it does not prove LinkedIn's own
// page still looks like that.
//
//   1. serve core/tests/fixtures/browser_site on 127.0.0.1:8791   (python -m http.server 8791 --bind 127.0.0.1)
//   2. start aqua-core with AQUA_SOCIAL_LINKEDIN_URL=http://127.0.0.1:8791/feed.html (target\debug\aqua-core.exe, repo root)
//   3. node e2e/flows-social.e2e.mjs

import { readFileSync, existsSync, readdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const CORE = 'http://127.0.0.1:8767';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();
const uploads = path.join(root, 'data', 'browser', 'uploads');

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

const flow = (name, text, approval = 'always') => ({
  name,
  settings: { approval },
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'cover', kind: 'image_generation', name: 'Draw the cover', config: { provider: 'card', title: 'Five ways agents save you time', style: 'ocean', size: '1200x627' } },
    { id: 'post', kind: 'social_media', name: 'Post on LinkedIn', config: { platform: 'linkedin', action: 'publish', text, hashtags: 'ai agents', media: '{{nodes.cover.image}}' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'posted: {{nodes.post.post_url}}' } },
  ],
  edges: [{ from: 'start', to: 'cover' }, { from: 'cover', to: 'post' }, { from: 'post', to: 'say' }],
});

try {
  // ---- the flow validates, and says what it will do -------------------------------------------------------------------
  const good = (await api('POST', '/api/v1/flows', flow('e2e-social-good', 'Agents can save you real time.'))).json;
  const report = (await api('GET', `/api/v1/flows/${good.id}/validate`)).json;
  check('the flow validates, and says Aqua will ask before posting', !report.issues.some((i) => i.level === 'error') && report.needs_approval.includes('post') && report.issues.some((i) => i.code === 'will_ask'), JSON.stringify(report.issues.map((i) => i.code)));

  // ---- it asks first, then posts ---------------------------------------------------------------------------------------
  const r1 = (await api('POST', `/api/v1/flows/${good.id}/run`, {})).json.run.id;
  await until('the approval', async () => (await run(r1)).run.status === 'waiting');
  const w = await run(r1);
  const gate = w.nodes.find((n) => n.status === 'waiting');
  check('it drew the cover first, then stopped at the post and asked', gate?.node_id === 'post' && w.nodes.find((n) => n.node_id === 'cover').status === 'succeeded', JSON.stringify(w.nodes.map((n) => [n.node_id, n.status])));
  check('the approval shows the platform and the words', /linkedin/.test(JSON.stringify(gate.wait)) && /Agents can save you real time/.test(JSON.stringify(gate.wait)), JSON.stringify(gate.wait));
  await api('POST', `/api/v1/flows/runs/${r1}/respond`, { node: 'post', decision: 'approved' });
  await until('the run to finish', async () => ['succeeded', 'failed'].includes((await run(r1)).run.status));
  const done = await run(r1);
  const post = done.nodes.find((n) => n.node_id === 'post');
  check('the post went through in real Edge', done.run.status === 'succeeded' && post.status === 'succeeded', `${done.run.status} ${done.run.error} ${post.error}`);
  check('the platform confirmed it, so the status says so', /^published: /.test(post.outputs.status), JSON.stringify(post.outputs));
  check('the link to the post was read from the page and made whole', post.outputs.post_url === 'http://127.0.0.1:8791/post-1.html', post.outputs.post_url);
  const stage = readdirSync(uploads).filter((f) => !uploadsBefore.has(f));
  check('the image was copied to the uploads folder and attached from there', stage.some((f) => /^post_.*\.png$/.test(f)), stage.join(','));
  const events = (await api('GET', `/api/v1/flows/runs/${r1}/events?limit=500`)).json.events.map((e) => e.message);
  check('the words of the post are not copied into the run log', events.every((m) => !m.includes('Agents can save you real time')));
  const verdict = (await api('GET', `/api/v1/flows/runs/${r1}/recovery`)).json.verdict;
  check('the health verdict counts it as a real success (there is proof)', verdict.kind === 'success', JSON.stringify(verdict));

  // ---- pressed, but the page never says it is live: not success -------------------------------------------------------------
  const quiet = (await api('POST', '/api/v1/flows', flow('e2e-social-silent', 'This one never confirms [fail-silently]', 'never'))).json.id;
  const r2 = (await api('POST', `/api/v1/flows/${quiet}/run`, {})).json.run.id;
  await until('the step to stop', async () => (await run(r2)).nodes.some((n) => n.node_id === 'post' && n.status === 'needs_action'));
  const q = await run(r2);
  const qp = q.nodes.find((n) => n.node_id === 'post');
  check('with no confirmation the step stops for a person instead of claiming success', qp.status === 'needs_action' && /not known whether it was published/.test(qp.error) && !qp.outputs.post_url, JSON.stringify(qp));
  await until('the run to wait', async () => (await run(r2)).run.status === 'waiting');
  check('and the run is waiting, not finished', true);
  const shown = (await api('GET', '/api/v1/flows/inbox')).json.items.find((i) => i.run_id === r2);
  check('it is in the inbox with the choices, including "it was done"', shown && JSON.stringify(shown.wait).includes('done'), JSON.stringify(shown));
  await api('POST', `/api/v1/flows/runs/${r2}/stop`);

  // ---- other platforms are said not to be built, not faked ----------------------------------------------------------------------
  const insta = { name: 'e2e-social-insta', settings: { approval: 'never' }, nodes: [{ id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } }, { id: 'post', kind: 'social_media', name: 'Post on Instagram', config: { platform: 'instagram', action: 'publish', text: 'hi', media: 'x.png' } }], edges: [{ from: 'start', to: 'post' }] };
  const i = (await api('POST', '/api/v1/flows', insta)).json.id;
  const vi = (await api('GET', `/api/v1/flows/${i}/validate`)).json;
  check('another platform is flagged as not built when the flow is checked', vi.issues.some((x) => x.code === 'platform_not_built'), JSON.stringify(vi.issues.map((x) => x.code)));
  const r3 = (await api('POST', `/api/v1/flows/${i}/run`, {})).json.run.id;
  await until('the instagram run to fail', async () => (await run(r3)).run.status === 'failed');
  check('and a real run stops without doing anything', /not built yet/.test((await run(r3)).nodes.find((n) => n.node_id === 'post').error));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  if (existsSync(uploads)) for (const f of readdirSync(uploads)) if (!uploadsBefore.has(f)) rmSync(path.join(uploads, f), { force: true });
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
