// Live test of the image step: a real run draws a real card with the system font, the run panel shows it, and
// a step that needs a generator that is not set up says so instead of making a placeholder.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root, without AQUA_IMAGE_ENDPOINT)
//   2. start the UI               (npm run dev, port 1420)
//   3. node e2e/flows-image.e2e.mjs
//
// It deletes its flows. The picture it draws stays in data/generated_images (that is where run images live).

import { createRequire } from 'node:module';
import { readFileSync, existsSync, statSync } from 'node:fs';
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
  const r = await fetch(`${CORE}${url}`, { method, headers: { Authorization: `Bearer ${uiToken}`, 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null), res: r };
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

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));

const card = {
  name: 'e2e-image-card',
  variables: [{ name: 'headline', label: 'Headline', kind: 'text', required: true, default: 'Agents that save you time' }],
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'cover', kind: 'image_generation', name: 'Draw the cover', config: { provider: 'card', title: '{{vars.headline}}', subtitle: 'Weekly AI digest', style: 'sunset', size: '1200x627' } },
    { id: 'say', kind: 'notification', name: 'Tell me', config: { channel: 'in_app', message: 'made {{nodes.cover.image}}' } },
  ],
  edges: [{ from: 'start', to: 'cover' }, { from: 'cover', to: 'say' }],
};
const missing = {
  name: 'e2e-image-missing',
  nodes: [
    { id: 'start', kind: 'trigger', name: 'Start', config: { trigger_type: 'manual' } },
    { id: 'pic', kind: 'image_generation', name: 'Draw a robot', config: { provider: 'diffusion', prompt: 'a friendly robot' } },
  ],
  edges: [{ from: 'start', to: 'pic' }],
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1200 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  const c = (await api('POST', '/api/v1/flows', card)).json.id;
  const start = await api('POST', `/api/v1/flows/${c}/run`, { inputs: { headline: 'Five ways agents save you time' } });
  const rid = start.json.run.id;
  await until('the run', async () => ['succeeded', 'failed'].includes((await api('GET', `/api/v1/flows/runs/${rid}`)).json.run.status));
  const detail = (await api('GET', `/api/v1/flows/runs/${rid}`)).json;
  const step = detail.nodes.find((n) => n.node_id === 'cover');
  check('the image step succeeded in a real run', detail.run.status === 'succeeded' && step.status === 'succeeded', `${detail.run.error} ${step.error}`);
  const file = step.outputs.image;
  check('it wrote a real PNG file where run images live', typeof file === 'string' && existsSync(file) && statSync(file).size > 5000 && /generated_images/.test(file), String(file));
  check('and a description for people who cannot see it', /Five ways agents save you time/.test(step.outputs.alt_text) && /Weekly AI digest/.test(step.outputs.alt_text), step.outputs.alt_text);
  const art = await fetch(`${CORE}/api/v1/flows/runs/${rid}/artifact?node=cover&key=image`, { headers: { Authorization: `Bearer ${uiToken}` } });
  const bytes = Buffer.from(await art.arrayBuffer());
  check('the artifact is served as an image', art.status === 200 && art.headers.get('content-type') === 'image/png' && bytes.subarray(1, 4).toString() === 'PNG');
  const other = await fetch(`${CORE}/api/v1/flows/runs/${rid}/artifact?node=say&key=image`, { headers: { Authorization: `Bearer ${uiToken}` } });
  check('a step that made no image has nothing to serve', other.status === 404);

  // ---- the run panel shows the picture -------------------------------------------------------------------------
  await page.goto(UI);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-image-card').first().waitFor();
  await page.locator('div', { has: page.getByText('e2e-image-card', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Open/ }) }).last().getByRole('button', { name: /Open/ }).click();
  await page.locator('[data-node="cover"]').waitFor();
  await page.getByRole('tab', { name: /^Run/ }).click();
  await page.getByText(new RegExp(`#${rid}`)).first().click();
  await page.getByTestId('run-panel').locator('[data-step="cover"]').click();
  const img = page.getByTestId('step-image');
  await img.waitFor();
  const natural = await until('the image to load', async () => (await img.evaluate((el) => (el.complete && el.naturalWidth > 0 ? [el.naturalWidth, el.naturalHeight] : null))));
  check('the run panel shows the picture that was drawn', natural[0] === 1200 && natural[1] === 627, JSON.stringify(natural));
  check('with its description as the alternative text', /Five ways agents save you time/.test((await img.getAttribute('alt')) ?? ''));

  // ---- a generator that is not set up is said, not faked ------------------------------------------------------------
  const m = (await api('POST', '/api/v1/flows', missing)).json.id;
  const r2 = (await api('POST', `/api/v1/flows/${m}/run`, {})).json.run.id;
  await until('the failure', async () => (await api('GET', `/api/v1/flows/runs/${r2}`)).json.run.status === 'failed');
  const failed = (await api('GET', `/api/v1/flows/runs/${r2}`)).json;
  const err = failed.nodes.find((n) => n.node_id === 'pic').error;
  check('with no generator the step says how to set one up and makes nothing', /no image generator is set up/.test(err) && /AQUA_IMAGE_ENDPOINT/.test(err) && !failed.nodes.find((n) => n.node_id === 'pic').outputs.image, err);
  const rec = (await api('GET', `/api/v1/flows/runs/${r2}/recovery`)).json;
  check('and it is not offered a pointless retry', !rec.steps[0].suggestions.some((s) => s.key === 'more_retries'), JSON.stringify(rec.steps[0].suggestions.map((s) => s.key)));
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
