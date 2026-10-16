// Live test of "approve and post" in real Edge against the LinkedIn-SHAPED stand-in page (post-1.html, whose reply box
// only adds the reply to that page). Nothing goes to LinkedIn. It drafts with the REAL local model, then a person edits
// the draft in the UI and presses "Approve and post"; the reply must appear under THAT comment, exactly once.
//
//   1. serve core/tests/fixtures/browser_site on 127.0.0.1:8791   (python -m http.server 8791 --bind 127.0.0.1)
//   2. start aqua-core (target\debug\aqua-core.exe from the repo root), wait for the model to load
//   3. start the UI (npm run dev, port 1420)
//   4. node e2e/flows-reply.e2e.mjs
// Restart the stand-in server between runs: the reply it shows lives only in that page's memory, so a reload clears it.

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');
const UI = process.env.AQUA_UI_URL ?? 'http://127.0.0.1:1420';
const CORE = 'http://127.0.0.1:8767';
const POST = 'http://127.0.0.1:8791/post-1.html';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();
const apiToken = readFileSync(path.join(root, 'data', '.aqua_api_token'), 'utf8').trim();

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};
const api = async (method, url, body, token = uiToken) => {
  const r = await fetch(`${CORE}${url}`, { method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null) };
};
const until = async (what, fn, ms = 600000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`timed out: ${what}`);
};
const run = (id) => api('GET', `/api/v1/flows/runs/${id}`).then((r) => r.json);
const drafts = async () => (await api('GET', '/api/v1/flows/comments/drafts')).json.drafts;

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));
const oldDrafts = new Set((await drafts()).map((d) => d.id));
const mine = async () => (await drafts()).filter((d) => !oldDrafts.has(d.id));

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1300 } });
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

const start = { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } };
const read = { id: 'read', kind: 'social_media', name: 'Read the comments', config: { platform: 'linkedin', action: 'read_comments', post_url: POST, my_name: 'Asha Rao' } };
const draft = { id: 'drafts', kind: 'comment_replies', name: 'Draft replies', config: { comments: '{{nodes.read.comments}}', post_text: 'Five ways AI agents save you time.', post_url: POST, my_name: 'Asha Rao', max_drafts: 1, facts: 'The owner schedules posts with Aqua, a local desktop assistant.' } };
const draftFlow = { name: 'e2e-reply', settings: { approval: 'always' }, nodes: [start, read, draft], edges: [{ from: 'start', to: 'read' }, { from: 'read', to: 'drafts' }] };
const readFlow = { name: 'e2e-reply-check', settings: { approval: 'always' }, nodes: [start, read], edges: [{ from: 'start', to: 'read' }] };
const go = async (id) => {
  const rid = (await api('POST', `/api/v1/flows/${id}/run`, {})).json.run.id;
  await until('the run', async () => ['succeeded', 'failed', 'waiting'].includes((await run(rid)).run.status));
  return run(rid);
};
const commentsNow = async (id) => (await go(id)).nodes.find((n) => n.node_id === 'read').outputs.comments;

try {
  const made = (await api('POST', '/api/v1/flows', draftFlow)).json;
  const r1 = await go(made.id);
  check('the drafting run finished by itself', r1.run.status === 'succeeded', `${r1.run.status} ${r1.run.error} ${r1.nodes.map((n) => n.error).filter(Boolean).join(' | ')}`);
  const inbox = await mine();
  check('one draft is waiting in the inbox, for Ravi (the first comment nobody answered)', inbox.length === 1 && inbox[0].author === 'Ravi Kumar' && inbox[0].status === 'pending', JSON.stringify(inbox));
  const ravi = inbox[0];
  const others = async () => (await mine()).filter((d) => d.id !== ravi.id);
  console.log(`  the model wrote: "${ravi?.reply}"`);

  const again = await go(made.id);
  const out2 = again.nodes.find((n) => n.node_id === 'drafts').outputs;
  check('reading the same post again does not draft the same comment twice', (await mine()).filter((d) => d.author === 'Ravi Kumar').length === 1 && (out2.skipped ?? []).some((x) => /already has a draft in your inbox/.test(x.reason)), JSON.stringify(out2.skipped));

  const denied = await api('POST', `/api/v1/flows/comments/drafts/${ravi.id}/approve`, {}, apiToken);
  check('the API token cannot approve a reply', denied.status === 403 && (await mine()).find((d) => d.id === ravi.id).status === 'pending', String(denied.status));

  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  const card = page.locator('[data-testid="comment-draft"]').filter({ hasText: 'Ravi Kumar' }).first();
  await card.waitFor();
  check('the inbox shows the comment, the draft and how many are waiting', (await page.getByTestId('pending-count').innerText()).includes('waiting') && (await card.innerText()).includes('Which tools do you use'));
  const mineText = 'Thanks Ravi. I schedule them with Aqua, running on my own computer.';
  await card.getByLabel('Reply to Ravi Kumar').fill(mineText);
  await card.getByRole('button', { name: 'Approve and post' }).click();
  await until('the reply to be posted', async () => !['pending', 'posting'].includes((await mine()).find((d) => d.id === ravi.id).status), 120000);
  const done = (await mine()).find((d) => d.id === ravi.id);
  check('it was posted, as the person edited it, and the draft as written is kept', done.status === 'posted' && done.sent_text === mineText && done.reply !== mineText, JSON.stringify(done));

  const twice = await api('POST', `/api/v1/flows/comments/drafts/${ravi.id}/approve`, {});
  check('approving it again is refused', twice.status === 409, String(twice.status));

  const events = (await api('GET', `/api/v1/flows/runs/${r1.run.id}/events?limit=200`)).json.events.map((e) => e.message).join(' | ');
  check('no comment or reply text is in the run log', !events.includes('Which tools do you use') && !events.includes(mineText));
  check('no page errors in the UI', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
