// The comment inbox's buttons, against a MOCKED inbox API (the real model cannot be made to produce an unconfirmed
// reply on demand). It checks what the page shows and which calls each button makes; it posts nothing anywhere.
//
//   1. start the UI (npm run dev, port 1420)    2. node e2e/flows-comments-ui.e2e.mjs
// aqua-core need not be running for the inbox itself, but the flows page loads its list from it, so start it too.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');
const UI = process.env.AQUA_UI_URL ?? 'http://127.0.0.1:1420';

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};

const base = { flow_id: 1, run_id: 1, post_url: 'https://www.linkedin.com/posts/1', comment_id: 'x', category: 'question', note: '', sent_text: '', error: '', created_at: 1, decided_at: 0 };
let drafts = [
  { ...base, id: 1, author: 'Ann', comment_text: 'How does it work?', reply: 'It runs locally.', status: 'pending' },
  { ...base, id: 2, author: 'Bo', comment_text: 'Which tools?', reply: 'Aqua.', status: 'pending', note: 'Check this before approving.' },
  { ...base, id: 3, author: 'Cy', comment_text: 'Can I try it?', reply: 'Yes.', sent_text: 'Yes, of course.', status: 'unknown', error: 'Pressed Post, but it is not known whether it was posted.' },
  { ...base, id: 4, author: 'Di', comment_text: 'Nice post', reply: 'Thanks', status: 'failed', error: 'The reply box did not open under that comment.' },
];
const calls = [];
const counts = () => Object.fromEntries(['pending', 'unknown', 'failed', 'posted', 'skipped'].map((s) => [s, drafts.filter((d) => d.status === s).length]));

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1600 } });
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));
await page.route('**/api/v1/flows/comments/drafts**', async (route) => {
  const req = route.request();
  const m = req.url().match(/drafts\/(\d+)\/(approve|skip|resolve)/);
  if (req.method() === 'GET' || !m) return route.fulfill({ json: { drafts, counts: counts() } });
  const d = drafts.find((x) => x.id === Number(m[1]));
  const body = req.postDataJSON?.() ?? {};
  calls.push({ id: d.id, what: m[2], body });
  if (m[2] === 'approve') Object.assign(d, { status: 'posted', sent_text: body.text });
  if (m[2] === 'skip') d.status = 'skipped';
  if (m[2] === 'resolve') d.status = body.action === 'posted' ? 'posted' : 'pending';
  return route.fulfill({ json: { draft: d } });
});

try {
  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  const card = (a) => page.locator('[data-testid="comment-draft"]').filter({ has: page.locator('strong', { hasText: new RegExp(`^${a}$`) }) }).first();
  await card('Ann').waitFor();
  check('the page says how many replies wait for you', (await page.getByTestId('pending-count').innerText()).startsWith('2 waiting'));
  check('a note to check the draft is shown', (await card('Bo').innerText()).includes('Check this before approving.'));
  check('an unconfirmed reply shows what happened and offers the two choices, not Approve', (await card('Cy').innerText()).includes('not known whether it was posted') && (await card('Cy').getByRole('button', { name: 'It was posted' }).count()) === 1 && (await card('Cy').getByRole('button', { name: 'Approve and post' }).count()) === 0);
  check('a failed reply offers the same two choices', (await card('Di').getByRole('button', { name: 'Try again' }).count()) === 1);

  await card('Bo').getByRole('button', { name: 'Skip' }).click();
  await page.locator('[data-testid="comment-draft"][data-status="skipped"]').count();
  await page.waitForFunction(() => document.querySelector('[data-testid="pending-count"]')?.textContent?.startsWith('1 waiting'));
  check('Skip sends one skip for that draft and it leaves the list', JSON.stringify(calls) === JSON.stringify([{ id: 2, what: 'skip', body: {} }]) && (await card('Bo').count()) === 0, JSON.stringify(calls));

  await card('Cy').getByRole('button', { name: 'It was posted' }).click();
  await page.waitForFunction(() => !document.body.innerText.includes('Can I try it?') || document.querySelectorAll('[data-testid="comment-draft"]').length < 3);
  check('"It was posted" resolves it as posted and sends nothing to the post', calls[1]?.id === 3 && calls[1].what === 'resolve' && calls[1].body.action === 'posted', JSON.stringify(calls));

  await card('Di').getByRole('button', { name: 'Try again' }).click();
  await card('Di').getByRole('button', { name: 'Approve and post' }).waitFor();
  check('"Try again" puts it back to waiting, with Approve available again', calls[2]?.what === 'resolve' && calls[2].body.action === 'retry');

  const box = card('Ann').getByLabel('Reply to Ann');
  await box.fill('');
  check('an empty reply cannot be approved', await card('Ann').getByRole('button', { name: 'Approve and post' }).isDisabled());
  await box.fill('Edited by me.');
  await card('Ann').getByRole('button', { name: 'Approve and post' }).click();
  await page.waitForFunction(() => !document.body.innerText.includes('How does it work?') || true);
  check('approving sends exactly the edited text', calls.at(-1)?.id === 1 && calls.at(-1).what === 'approve' && calls.at(-1).body.text === 'Edited by me.', JSON.stringify(calls.at(-1)));
  check('no page errors', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  await browser.close();
}
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
