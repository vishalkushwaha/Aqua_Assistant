// Browser test for the AI Flows canvas, run against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/flows.e2e.mjs
//
// Every UI action is confirmed through the API, so "it was saved" means the database changed, not only
// the page. The test creates flows from a template and deletes every flow it created; it never touches
// a flow that existed before it started. Nothing outside Aqua's own database is changed.

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

const until = async (what, fn, ms = 15000) => {
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

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await browser.newPage({ viewport: { width: 1500, height: 1000 } });
page.on('dialog', (d) => d.accept());
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  await page.goto(UI);
  // A full-screen first-run overlay (model onboarding) can cover the app; it is not what is tested here.
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const s = getComputedStyle(el);
      if (s.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByRole('heading', { name: 'AI Flows' }).waitFor();
  check('the AI Flows tab lists the templates', await page.getByText('AI social post').first().waitFor({ timeout: 8000 }).then(() => true, () => false));

  // ---- open a template ---------------------------------------------------------------------------
  const tpl = page.locator('div', { has: page.getByText('AI social post', { exact: true }) }).filter({ has: page.getByRole('button', { name: /Use this template/ }) }).last();
  await tpl.getByRole('button', { name: /Use this template/ }).click();
  await page.locator('[data-node="publish"]').waitFor();
  const flow = (await created())[0];
  check('the template becomes a stored flow and opens on the canvas', !!flow && flow.node_count === 14, JSON.stringify(flow));
  check('every step is drawn as a node', (await page.locator('[data-node]').count()) === 14);
  check('the connections are drawn', (await page.locator('path[data-edge]').count()) === 15);
  await page.getByTestId('side-panel').getByText('Ready to run').waitFor();
  check('a valid flow says it is ready to run', true);

  // ---- edit a step from its form --------------------------------------------------------------------------
  await page.locator('[data-node="publish"]').click({ position: { x: 60, y: 20 } });
  await page.getByLabel('Platform').selectOption('instagram');
  await page.getByLabel('Step name').fill('Publish to Instagram');
  await page.getByLabel('Change note').fill('use Instagram');
  await page.getByRole('button', { name: /^Save$/ }).click();
  const v2 = await until('version 2', async () => {
    const f = (await api('GET', `/api/v1/flows/${flow.id}`)).json;
    return f.version === 2 ? f : null;
  });
  const pub = v2.nodes.find((n) => n.id === 'publish');
  check('editing a step saves a new version with the change', pub.config.platform === 'instagram' && pub.name === 'Publish to Instagram');
  const vs = (await api('GET', `/api/v1/flows/${flow.id}/versions`)).json.versions;
  check('the version records its note and author', vs[0].note === 'use Instagram' && vs[0].author === 'user');

  // ---- drag a node ----------------------------------------------------------------------------------------------
  const research = page.locator('[data-node="research"]');
  await research.scrollIntoViewIfNeeded();
  const b0 = await research.boundingBox();
  await page.mouse.move(b0.x + 60, b0.y + 20);
  await page.mouse.down();
  await page.mouse.move(b0.x + 260, b0.y + 20, { steps: 6 });
  await page.mouse.up();
  const b1 = await research.boundingBox();
  check('dragging moves a node', Math.round(b1.x - b0.x) >= 150, `${b0.x} -> ${b1.x}`);
  await page.getByRole('button', { name: /^Save$/ }).click();
  await until('positions saved', async () => (await api('GET', `/api/v1/flows/${flow.id}`)).json.nodes.find((n) => n.id === 'research').position.x > 200);
  check('a move is saved without creating a version', (await api('GET', `/api/v1/flows/${flow.id}`)).json.version === 2);

  // ---- add a step, it is connected to the selected one; delete it again ---------------------------------------------
  await page.locator('[data-node="ask_schedule"]').click({ position: { x: 60, y: 20 } });
  await page.getByLabel('Add a step').selectOption('notification');
  await until('a new node', async () => (await page.locator('[data-node]').count()) === 15);
  check('a step added after a selected one is connected to it', (await page.locator('path[data-edge]').count()) === 16);
  await page.getByLabel('Step name').fill('Tell me');
  await page.getByRole('button', { name: /Delete$/ }).click();
  await until('node removed', async () => (await page.locator('[data-node]').count()) === 14);
  check('deleting a step removes its connections too', (await page.locator('path[data-edge]').count()) === 15);

  // ---- connect two steps by dragging from an exit handle ------------------------------------------------------------
  await page.getByTestId('flow-canvas').click({ position: { x: 5, y: 5 } });
  await page.getByLabel('Add a step').selectOption('notification');
  await until('an unconnected node', async () => (await page.locator('[data-node]').count()) === 15);
  check('a step added with nothing selected is not connected', (await page.locator('path[data-edge]').count()) === 15);
  const handle = page.locator('[data-port="ask_schedule:out"]');
  const target = page.locator('[data-node="notification"]');
  await target.scrollIntoViewIfNeeded();
  const hb = await handle.boundingBox();
  const tb = await target.boundingBox();
  await page.mouse.move(hb.x + 7, hb.y + 7);
  await page.mouse.down();
  await page.mouse.move(tb.x + 100, tb.y + 30, { steps: 8 });
  await page.mouse.up();
  await until('a connection', async () => (await page.locator('path[data-edge]').count()) === 16);
  check('dragging from an exit handle to a step connects them', true);
  await page.getByRole('button', { name: /Delete$/ }).click();
  await until('cleaned up', async () => (await page.locator('[data-node]').count()) === 14);

  // ---- validation follows the canvas ----------------------------------------------------------------------------------
  await page.locator('[data-node="research"]').click({ position: { x: 60, y: 20 } });
  await page.getByRole('button', { name: /Delete$/ }).click();
  await page.getByText(/problems? to fix/).waitFor({ timeout: 8000 });
  const flagged = await page.locator('[data-node="write_post"]').innerText();
  check('deleting a step that others use shows the problem and marks the step', /✕/.test(flagged), flagged);
  check('the problem is explained in words', await page.getByText(/does not exist/).first().isVisible());

  // ---- versions: compare and restore ---------------------------------------------------------------------------------
  await page.getByRole('button', { name: /^Save$/ }).click();
  await until('version 3', async () => (await api('GET', `/api/v1/flows/${flow.id}`)).json.version === 3);
  await page.getByRole('button', { name: /Versions/ }).click();
  await page.getByRole('button', { name: /v1 · user/ }).click();
  await page.getByText(/Changes since v1/).waitFor();
  const diffText = await page.getByTestId('side-panel').innerText();
  check('a version comparison lists what changed in words', /Removed: Research the topic/.test(diffText) && /Changed: Publish to Instagram/.test(diffText), diffText.slice(0, 400));
  await page.getByRole('button', { name: /Restore v1/ }).click();
  const v4 = await until('restore', async () => {
    const f = (await api('GET', `/api/v1/flows/${flow.id}`)).json;
    return f.version === 4 ? f : null;
  });
  check('restoring an old version makes it current as a new version', v4.nodes.some((n) => n.id === 'research') && v4.nodes.find((n) => n.id === 'publish').config.platform === 'linkedin');
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
