// Browser test for the Agent Learning page, run against the real UI and the real aqua-core.
//
//   1. start aqua-core            (target\release\aqua-core.exe)
//   2. start the UI               (npm run dev, port 1420; its proxy adds the tokens)
//   3. node e2e/agent-learning.e2e.mjs
//
// It uses Playwright from tools/browser-mcp (already installed for the managed browser) and Edge.
// Every UI action is confirmed through the API, so "the toggle persists" means the database changed,
// not only the page. The test creates and removes one experience of its own and puts every setting
// back the way it found it. It never touches experiences it did not create.

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
const token = (file) => readFileSync(path.join(root, 'data', file), 'utf8').trim();
const uiToken = token('.aqua_ui_token');
const apiToken = token('.aqua_api_token');

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};

const api = async (method, url, body, tokenValue = uiToken) => {
  const r = await fetch(`${CORE}${url}`, {
    method,
    headers: { Authorization: `Bearer ${tokenValue}`, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: r.status, json: await r.json().catch(() => null) };
};

// A verified experience of the test's own, inserted with the same store the agent uses (through the
// documented settings-free path: the database file). Removed again at the end.
import { execFileSync } from 'node:child_process';
function seedExperience(problem) {
  const script = `
import sqlite3, time, sys
c = sqlite3.connect(r'${path.join(root, 'data', 'experience.db').replace(/\\/g, '\\\\')}', timeout=10)
now = int(time.time())
c.execute("""INSERT INTO agent_experiences (scope, project_key, owner_id, task_type, experience_type, problem_summary, context_summary, approach,
  important_steps, tools_used, constraints, technologies, solution_summary, verification_method, verification_result, verified, result, success,
  confidence, quality_score, status, human_approved, embedding_model, embedding_version, embedding, created_at, updated_at, last_verified_at)
  VALUES ('GLOBAL','','','live_data','SOLUTION',?, 'browser task', 'open a search then read the page', '["browser_navigate to a search"]', '["browser_navigate"]',
  '[]', '["browser"]', 'quote figures from the page', 'page_evidence', 'figures found on the page', 1, 'ok', 1, 0.5, 0.7, 'VERIFIED', 0, 'aqua-hash-384', 1, NULL, ?, ?, ?)""",
  (sys.argv[1], now, now, now))
c.commit()
print(c.execute("select max(id) from agent_experiences").fetchone()[0])
`;
  return Number(execFileSync('python', ['-c', script, problem], { encoding: 'utf8' }).trim());
}

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const page = await (await browser.newContext({ viewport: { width: 1400, height: 1000 } })).newPage();
const original = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
let seededId = null;

try {
  await page.goto(UI, { waitUntil: 'domcontentloaded' });
  await page.getByRole('button', { name: /Agent Learning/ }).first().click();
  await page.getByRole('heading', { name: 'Agent Learning' }).waitFor({ timeout: 10000 });
  check('the Agent Learning page opens from the sidebar', true);

  // ---- settings load ----------------------------------------------------------------------------
  const learningSwitch = page.getByRole('switch', { name: 'Agent Learning' });
  await learningSwitch.waitFor();
  check('all four switches are shown', (await page.getByRole('switch').count()) >= 4);
  check('the switch shows the stored state', (await learningSwitch.getAttribute('aria-checked')) === String(original.learning_enabled));

  // ---- toggle persists --------------------------------------------------------------------------
  await learningSwitch.click();
  await page.waitForFunction((v) => document.querySelector('[role=switch][aria-label="Agent Learning"]')?.getAttribute('aria-checked') === v, String(!original.learning_enabled), { timeout: 8000 });
  let stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('toggling Learning is saved in the database, not only on the page', stored.learning_enabled === !original.learning_enabled, JSON.stringify(stored));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('button', { name: /Agent Learning/ }).first().click();
  await page.getByRole('switch', { name: 'Agent Learning' }).waitFor();
  check('the setting survives a page reload', (await page.getByRole('switch', { name: 'Agent Learning' }).getAttribute('aria-checked')) === String(!original.learning_enabled));
  await page.getByRole('switch', { name: 'Agent Learning' }).click();
  await page.waitForFunction((v) => document.querySelector('[role=switch][aria-label="Agent Learning"]')?.getAttribute('aria-checked') === v, String(original.learning_enabled), { timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('toggling back restores it', stored.learning_enabled === original.learning_enabled);

  // Autonomous experimentation is its own switch and off by default.
  const exp = page.getByRole('switch', { name: 'Autonomous Experimentation' });
  check('experimentation is a separate switch', (await exp.getAttribute('aria-checked')) === String(original.autonomous_experimentation));

  // Auto practice is its own switch, off by default, and its button waits for it.
  const practiceSwitch = page.getByRole('switch', { name: 'Auto Practice' });
  check('auto practice is a separate switch', (await practiceSwitch.getAttribute('aria-checked')) === String(original.auto_practice_enabled));
  check('auto practice is off unless someone turned it on', original.auto_practice_enabled === false, 'it was already on before the test');
  const runNow = page.getByRole('button', { name: 'Run one now' });
  check('"Run one now" is disabled while practice is off', await runNow.isDisabled());
  await practiceSwitch.click();
  await page.waitForFunction(() => document.querySelector('[role=switch][aria-label="Auto Practice"]')?.getAttribute('aria-checked') === 'true', null, { timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('switching auto practice on is saved in the database', stored.auto_practice_enabled === true, JSON.stringify(stored));
  // The button follows the practice status, which refreshes just after the setting is saved.
  const buttonEnabled = await page
    .waitForFunction(() => { const b = [...document.querySelectorAll('button')].find((x) => x.textContent === 'Run one now'); return !!b && !b.disabled; }, null, { timeout: 8000 })
    .then(() => true, () => false);
  check('"Run one now" is enabled once practice is on', buttonEnabled);
  const practiceText = await page.locator('body').innerText();
  check('the practice card shows its limits and what it is waiting for', practiceText.includes('Auto practice') && /Today: \d+ of \d+/.test(practiceText));
  await practiceSwitch.click();
  await page.waitForFunction(() => document.querySelector('[role=switch][aria-label="Auto Practice"]')?.getAttribute('aria-checked') === 'false', null, { timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('switching it back off is saved', stored.auto_practice_enabled === false);

  // ---- statistics and status --------------------------------------------------------------------
  const stats = (await api('GET', '/api/v1/learning/stats', undefined, apiToken)).json;
  const text = await page.locator('body').innerText();
  check('the dashboard shows the statistics', text.includes('Tasks analyzed') && text.includes('Retrieval hit rate') && text.includes('Duplicates prevented'));
  check('the counts on screen match the API', new RegExp(`Tasks analyzed`).test(text) && text.includes(String(stats.tasks_analyzed)));
  check('vector store and embedding status are shown', /Vector store: \d+ indexed/.test(text) && text.includes('Embedding: aqua-hash-384'));
  check('the with/without-memory comparison is shown', text.includes('With memory vs without'));

  // ---- settings tab: change, save, persist ------------------------------------------------------
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  const attempts = page.locator('label', { hasText: 'Maximum attempts' }).locator('input');
  await attempts.fill('2');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await page.getByText('Settings saved.').waitFor({ timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('a saved limit is stored', stored.max_attempts === 2);
  await attempts.fill('0');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await page.getByText('Settings saved.').waitFor({ timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('a limit cannot be configured away (0 attempts becomes 1)', stored.max_attempts === 1, JSON.stringify(stored.max_attempts));

  const perDay = page.locator('label', { hasText: 'Practice tasks per day' }).locator('input');
  await perDay.fill('500');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await page.getByText('Settings saved.').waitFor({ timeout: 8000 });
  stored = (await api('GET', '/api/v1/learning/settings', undefined, apiToken)).json;
  check('the practice limits are editable and clamped (500 a day becomes 24)', stored.practice_max_per_day === 24, String(stored.practice_max_per_day));
  await perDay.fill(String(original.practice_max_per_day));
  await page.getByRole('button', { name: 'Save settings' }).click();
  await page.getByText('Settings saved.').waitFor({ timeout: 8000 });

  // ---- experiences: search, details, deprecate, revalidate, delete -----------------------------
  seededId = seedExperience('zz browser test experience about reading a quote page');
  await page.getByRole('button', { name: 'Experiences', exact: true }).click();
  const search = page.getByLabel('Search experiences');
  await search.fill('zz browser test');
  await page.getByText('zz browser test experience about reading a quote page').waitFor({ timeout: 8000 });
  check('experiences can be searched', (await page.getByText('zz browser test experience').count()) >= 1);
  await search.fill('nothing matches this text at all 12345');
  await page.getByText(/No experiences yet/).waitFor({ timeout: 8000 });
  check('a search with no match shows an empty list', true);
  await search.fill('zz browser test');
  await page.getByRole('button', { name: /zz browser test experience/ }).click();
  await page.getByText('Why it is (or is not) trusted').waitFor({ timeout: 8000 });
  const detail = await page.locator('body').innerText();
  check('the detail shows why it is trusted and its verification', detail.includes('Verified by page evidence') && detail.includes('figures found on the page'));

  await page.getByRole('button', { name: 'Deprecate' }).click();
  await page.getByText('Deprecated.', { exact: true }).waitFor({ timeout: 8000 });
  check('deprecating changes the stored status', (await api('GET', `/api/v1/learning/experiences/${seededId}`, undefined, apiToken)).json.experience.status === 'DEPRECATED');
  await page.getByRole('button', { name: 'Revalidate' }).click();
  await page.getByText('Revalidated.', { exact: true }).waitFor({ timeout: 8000 });
  check('revalidating restores a still-valid experience', (await api('GET', `/api/v1/learning/experiences/${seededId}`, undefined, apiToken)).json.experience.status === 'VERIFIED');
  await page.getByRole('button', { name: 'Approve / mark trusted' }).click();
  await page.getByText('Approved: now trusted.', { exact: true }).waitFor({ timeout: 8000 });
  check('a person can approve a verified experience', (await api('GET', `/api/v1/learning/experiences/${seededId}`, undefined, apiToken)).json.experience.status === 'TRUSTED');

  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await page.getByRole('button', { name: 'Yes, delete it' }).click();
  await page.waitForFunction(() => !document.body.innerText.includes('Why it is (or is not) trusted'), null, { timeout: 8000 });
  check('deleting needs a confirmation and removes it', (await api('GET', `/api/v1/learning/experiences/${seededId}`, undefined, apiToken)).status === 404);
  seededId = null;

  // ---- the API token cannot change what the UI can ----------------------------------------------
  const denied = await api('PUT', '/api/v1/learning/settings', original, apiToken);
  check('the API token cannot change settings (UI token only)', denied.status === 403, String(denied.status));

  // ---- activity ---------------------------------------------------------------------------------
  await page.getByRole('button', { name: 'Activity', exact: true }).click();
  await page.getByText('Recent events').waitFor();
  check('the activity tab shows jobs and events', (await page.getByText('Learning jobs').count()) === 1);
} catch (e) {
  check('the test ran to completion', false, e instanceof Error ? e.message : String(e));
} finally {
  if (seededId !== null) await api('POST', `/api/v1/learning/experiences/${seededId}/delete`).catch(() => undefined);
  await api('PUT', '/api/v1/learning/settings', original).catch(() => undefined);
  await browser.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed === 0 ? 0 : 1);
