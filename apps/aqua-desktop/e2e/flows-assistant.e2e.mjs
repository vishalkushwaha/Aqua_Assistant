// Browser test for the AI flow builder, run against the real UI and the real aqua-core.
//
//   1. start aqua-core with AQUA_FLOWS_MODEL=off   (only the exact rules; the language model can take minutes)
//   2. start the UI                                (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows-assistant.e2e.mjs
//
// It builds a flow from the specification's own example sentence, edits it by sentence, checks each
// proposal on the canvas before applying, applies and undoes a change, and answers a question. Every
// step is confirmed through the API. It deletes the flows it created and touches nothing else.

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

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));
const publishPlatform = (f) => f.nodes.find((n) => n.id === 'publish_linkedin')?.config.platform;

const SPEC = 'Create a LinkedIn post about the latest AI agent trends. Research the topic, create an engaging post, generate a professional image, and publish it to LinkedIn.';

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1100 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByTestId('composer').waitFor();

  // ---- a sentence becomes a flow, shown before anything is saved ------------------------------------------
  await page.getByLabel('Describe the flow').fill(SPEC);
  await page.getByRole('button', { name: /Build it/ }).click();
  await page.getByTestId('proposal').waitFor();
  const card = await page.getByTestId('proposal').innerText();
  check('the proposal names the flow and explains each step', /New flow: LinkedIn post/.test(card) && /Research the topic: You asked for current information/.test(card) && /Approve the post: Publishing cannot be undone/.test(card), card.slice(0, 300));
  check('it says the result is valid', /valid and ready to run/.test(card));
  check('it warns which account and route will be used', /linkedin\/main/.test(card) && /browser/.test(card));
  check('it asks about timing instead of assuming', /Publish as soon as you approve it, or schedule it/.test(card));
  check('the proposed flow is drawn before it is created', (await page.getByTestId('composer').locator('[data-node]').count()) >= 14);
  check('nothing has been stored yet', (await created()).length === 0);

  await page.getByRole('button', { name: /Create this flow/ }).click();
  const flow = await until('the flow to be created', async () => (await created())[0]);
  await page.getByRole('tab', { name: 'Assistant' }).waitFor();
  const v = (await api('GET', `/api/v1/flows/${flow.id}/versions`)).json.versions;
  check('accepting creates the flow, recorded as made by the assistant', !!flow && v[0].author === 'ai' && flow.node_count >= 14, JSON.stringify(v[0]));

  // ---- edit by sentence: preview, then apply, then undo -------------------------------------------------------------
  await page.getByRole('tab', { name: 'Assistant' }).click();
  await page.getByLabel('Ask the assistant').fill('Instead of posting on LinkedIn, post it on Instagram.');
  await page.getByRole('button', { name: 'Send' }).click();
  await page.getByTestId('proposal').waitFor();
  const changed = await page.locator('[data-node="publish_linkedin"]').innerText();
  check('the canvas previews the change before it is applied', /Publish to Instagram/.test(changed) && /changed/.test(changed), changed);
  check('only what names LinkedIn is marked as changed', (await page.locator('[data-node]', { hasText: 'changed' }).count()) <= 7 && !(await page.locator('[data-node="research"]').innerText()).includes('changed') && !(await page.locator('[data-node="preview"]').innerText()).includes('changed'));
  check('nothing is applied until it is accepted', publishPlatform((await api('GET', `/api/v1/flows/${flow.id}`)).json) === 'linkedin');
  check('the explanation says what was switched', /Switched from LinkedIn to Instagram/.test(await page.getByTestId('proposal').innerText()));
  await page.getByRole('button', { name: 'Apply this change' }).click();
  const v2 = await until('version 2', async () => {
    const f = (await api('GET', `/api/v1/flows/${flow.id}`)).json;
    return f.version === 2 ? f : null;
  });
  check('applying saves a new version with the platform switched', publishPlatform(v2) === 'instagram' && v2.nodes.find((n) => n.id === 'open_linkedin').config.url === 'https://www.instagram.com');
  await page.getByRole('button', { name: /Undo the last change/ }).click();
  const v3 = await until('undo', async () => {
    const f = (await api('GET', `/api/v1/flows/${flow.id}`)).json;
    return f.version === 3 ? f : null;
  });
  check('undo restores the earlier flow as a new version', publishPlatform(v3) === 'linkedin');

  // ---- add a platform, then dismiss ----------------------------------------------------------------------------------
  await page.getByLabel('Ask the assistant').fill('Add Twitter/X as another platform.');
  await page.getByRole('button', { name: 'Send' }).click();
  await page.getByTestId('proposal').waitFor();
  check('added steps are marked new on the canvas', (await page.locator('[data-node]', { hasText: 'new' }).count()) === 4);
  await page.getByRole('button', { name: 'Dismiss' }).click();
  check('dismissing changes nothing', (await api('GET', `/api/v1/flows/${flow.id}`)).json.version === 3 && (await page.locator('[data-node]').count()) === flow.node_count);

  // ---- what cannot be understood ---------------------------------------------------------------------------------------
  await page.getByLabel('Ask the assistant').fill('Make it purple.');
  await page.getByRole('button', { name: 'Send' }).click();
  await page.getByTestId('proposal').getByText(/I could not turn that into a change/).waitFor();
  check('an unrecognised request is said so and offers what it can do', await page.getByTestId('proposal').getByText(/switch the platform/).first().isVisible());
  check('and its Apply button is disabled', await page.getByRole('button', { name: 'Apply this change' }).isDisabled());
  await page.getByRole('button', { name: 'Dismiss' }).click();

  // ---- the assistant works on the saved flow ---------------------------------------------------------------------------------
  await page.getByRole('tab', { name: 'Step' }).click();
  await page.locator('[data-node="write_post"]').click({ position: { x: 60, y: 20 } });
  await page.getByLabel('Step name').fill('Write the post (edited)');
  await page.getByRole('tab', { name: 'Assistant' }).click();
  await page.getByLabel('Ask the assistant').fill('Remove the image.');
  await page.getByRole('button', { name: 'Send' }).click();
  await page.getByText(/Save your changes first/).waitFor();
  check('with unsaved edits it asks you to save first', true);
  await page.getByRole('button', { name: /^Save$/ }).click();
  await until('saved', async () => (await api('GET', `/api/v1/flows/${flow.id}`)).json.version === 4);
  const ch = (await api('GET', `/api/v1/flows/${flow.id}/chat`)).json.messages;
  check('the conversation is kept with the flow', ch.some((m) => m.role === 'user' && /Instagram/.test(m.text)) && ch.some((m) => m.role === 'assistant' && /Applied/.test(m.text)));

  // ---- a question is asked, and the answer completes the plan ---------------------------------------------------------------------
  await page.getByRole('button', { name: /← Flows/ }).click();
  await page.getByLabel('Describe the flow').fill('Post an image and description on LinkedIn.');
  await page.getByRole('button', { name: /Build it/ }).click();
  await page.getByText('What topic should it cover?').waitFor();
  check('a missing topic is asked for, not guessed', true);
  await page.getByLabel('What topic should it cover?').fill('AI safety');
  await page.getByLabel('What topic should it cover?').press('Enter');
  await until('the question to go away', async () => !(await page.getByText('What topic should it cover?').isVisible().catch(() => false)));
  check('answering it completes the proposal', /New flow: LinkedIn post/.test(await page.getByTestId('proposal').innerText()));
  await page.getByRole('button', { name: 'Dismiss' }).click();
  // ---- "every Monday for 4 weeks" becomes a real schedule when the flow is created ------------------------------------------------
  await page.getByLabel('Describe the flow').fill('Write a LinkedIn post about AI agents every Monday at 9 am for 4 weeks.');
  await page.getByRole('button', { name: /Build it/ }).click();
  await page.getByTestId('proposal-schedule').waitFor();
  const sched = await page.getByTestId('proposal-schedule').innerText();
  check('the proposal says it will create a schedule, in words, with how long it goes on', /every monday at 09:00/.test(sched) && /for 4 weeks/.test(sched) && /Schedule tab/.test(sched), sched);
  const beforeSchedules = (await api('GET', '/api/v1/flows/schedules')).json.schedules.length;
  const idsBefore = new Set((await created()).map((f) => f.id));
  await page.getByRole('button', { name: /Create this flow/ }).click();
  const made = await until('the scheduled flow to be created', async () => (await created()).find((f) => !idsBefore.has(f.id)));
  // The flow is visible a moment before its schedule (one request makes both).
  const mine = await until('its schedule', async () => (await api('GET', '/api/v1/flows/schedules')).json.schedules.find((x) => x.flow_id === made.id));
  const all = (await api('GET', '/api/v1/flows/schedules')).json.schedules;
  check('applying it created a real schedule for the new flow', all.length === beforeSchedules + 1 && mine && mine.when.cron === '0 9 * * 1' && mine.enabled === true, JSON.stringify({ made: made.id, before: beforeSchedules, all: all.map((x) => [x.id, x.flow_id]) }));
  check('which stops by itself after four weeks', mine.campaign.end_on && mine.ends_at > Math.floor(Date.now() / 1000), JSON.stringify(mine.campaign));
  check('and the flow\'s own start step is a plain manual one', (await api('GET', `/api/v1/flows/${made.id}`)).json.nodes.find((n) => n.id === 'start').config.trigger_type === 'manual');
  if (mine) await api('POST', `/api/v1/flows/schedules/${mine.id}/delete`);
  check('no script errors on the page', errors.length === 0, errors.join(' | '));
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
