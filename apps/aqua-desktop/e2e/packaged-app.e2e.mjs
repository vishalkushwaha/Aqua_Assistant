// What the PACKAGED app does, in a real browser, without Tauri: the built UI is served from its own origin (as the
// installed app is), a stub stands in for the desktop shell's `gateway_info` command, and the real Content-Security-
// Policy from tauri.conf.json is applied to the page. It proves that, with no Vite proxy in between:
//   - the page reaches the gateway on its own and sends the UI token (a plain GET and a PUT, which needs CORS),
//   - nothing the page does is blocked by the policy (no violation is reported),
//   - without the bridge the same page gets nothing (so the bridge, not luck, is what makes it work),
//   - no request goes to a font or any other outside host.
//
//   1. build the UI:      npx vite build --outDir ../../scratch/dist_pkg --emptyOutDir        (in apps/aqua-desktop)
//   2. serve it:          npx vite preview --outDir ../../scratch/dist_pkg --port 4173 --host 127.0.0.1
//   3. start aqua-core with AQUA_ALLOWED_ORIGINS=http://127.0.0.1:4173  (target\debug\aqua-core.exe, repo root)
//   4. node e2e/packaged-app.e2e.mjs

import { readFileSync, existsSync } from 'node:fs';
import http from 'node:http';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const require = createRequire(path.join(root, 'tools', 'browser-mcp', 'package.json'));
const { chromium } = require('playwright-core');
const PAGE = process.env.AQUA_PKG_URL ?? 'http://127.0.0.1:4173/';
const CORE = 'http://127.0.0.1:8767';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();
const csp = JSON.parse(readFileSync(path.join(root, 'apps', 'aqua-desktop', 'src-tauri', 'tauri.conf.json'), 'utf8')).app.security.csp;

// Serve the built UI as the installed app does: its own origin, with the policy sent as a response header.
const dist = path.join(root, 'scratch', 'dist_pkg');
const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' };
const server = http.createServer((req, res) => {
  const rel = decodeURIComponent((req.url ?? '/').split('?')[0]);
  let file = path.join(dist, rel === '/' ? 'index.html' : rel);
  if (!file.startsWith(dist) || !existsSync(file)) file = path.join(dist, 'index.html');
  const ext = path.extname(file);
  res.writeHead(200, { 'content-type': types[ext] ?? 'application/octet-stream', 'content-security-policy': csp });
  res.end(readFileSync(file));
});
await new Promise((r) => server.listen(4173, '127.0.0.1', r));

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};
const api = async (method, url, body) => {
  const r = await fetch(`${CORE}${url}`, { method, headers: { Authorization: `Bearer ${uiToken}`, 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null) };
};

const browser = await chromium.launch({ channel: 'msedge', headless: true });
const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const made = (await api('POST', '/api/v1/flows', { template: 'blank', name: 'e2e-packaged' })).json;

async function open({ withBridge }) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const page = await context.newPage();
  const seen = { requests: [], violations: [], consoleErrors: [] };
  page.on('request', (r) => seen.requests.push(`${r.method()} ${r.url()}`));
  page.on('console', (m) => {
    const t = m.text();
    if (/Content Security Policy|Refused to/i.test(t)) seen.violations.push(t);
    if (m.type() === 'error') seen.consoleErrors.push(t);
  });
  if (withBridge) {
    await page.addInitScript((token) => {
      window.__TAURI_INTERNALS__ = { invoke: async (cmd) => { if (cmd === 'gateway_info') return { base_url: 'http://127.0.0.1:8767', ui_token: token }; throw new Error('unknown command'); } };
    }, uiToken);
  }
  await page.goto(PAGE);
  // The first-run wizard covers the page; the other e2e scripts dismiss it the same way.
  await page.waitForTimeout(500);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('body *')) {
      const st = getComputedStyle(el);
      if (st.position === 'fixed' && el.offsetWidth >= innerWidth * 0.9 && el.offsetHeight >= innerHeight * 0.9 && !el.querySelector('main')) el.remove();
    }
  });
  return { page, context, seen };
}

try {
  // ---- with the bridge: the packaged path
  const { page, context, seen } = await open({ withBridge: true });
  await page.getByRole('button', { name: /^AI Flows$/ }).click();
  await page.getByText('e2e-packaged').first().waitFor({ timeout: 20000 });
  check('the page lists flows from the gateway without any proxy, so the token was sent', true);
  const gateway = seen.requests.filter((r) => r.includes('127.0.0.1:8767/api/'));
  check('its requests went straight to the gateway address, not to the page\'s own origin', gateway.length > 0 && !seen.requests.some((r) => r.includes('127.0.0.1:4173/api/')), seen.requests.filter((r) => r.includes('/api/')).slice(0, 3).join(' | '));
  const put = await page.evaluate(async ({ id }) => {
    const get = await (await fetch(`/api/v1/flows/${id}`)).json();
    const res = await fetch(`/api/v1/flows/${id}`, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ def: get, expected_version: get.version, note: 'saved from the packaged page' }) });
    return { status: res.status, body: await res.json().catch(() => null) };
  }, { id: made.id });
  check('a save (PUT, which needs CORS) from the other origin works and carries the UI token', put.status === 200, JSON.stringify(put));
  // A live run view reads the event stream. EventSource cannot send the token, so the page streams with fetch.
  const run = (await api('POST', `/api/v1/flows/${made.id}/run`, {})).json.run.id;
  const streamed = await page.evaluate(async ({ rid }) => {
    const res = await fetch(`/api/v1/flows/runs/${rid}/stream?after=0`, { headers: { Accept: 'text/event-stream' } });
    if (!res.ok || !res.body) return { ok: false, status: res.status };
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let text = '';
    const stop = Date.now() + 20000;
    while (Date.now() < stop) {
      const { done, value } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
      if (text.includes('event: end')) break;
    }
    return { ok: true, sawEvents: /event: (event|status)/.test(text), ended: text.includes('event: end') };
  }, { rid: run });
  check('the live event stream of a run can be read from the packaged page with the token (fetch, not EventSource)', streamed.ok && streamed.sawEvents && streamed.ended, JSON.stringify(streamed));
  check('the content policy blocked nothing the page needed', seen.violations.length === 0, seen.violations.join(' | '));
  check('nothing was requested from a font service or any other outside host', seen.requests.every((r) => /^(GET|POST|PUT|OPTIONS|DELETE) (http:\/\/127\.0\.0\.1:(4173|8767)\/|data:|blob:)/.test(r)), seen.requests.filter((r) => !/127\.0\.0\.1/.test(r)).join(' | '));
  await context.close();

  // ---- without it: the same page has no way in (so the bridge is what made the above work)
  const bare = await open({ withBridge: false });
  await bare.page.getByRole('button', { name: /^AI Flows$/ }).click().catch(() => undefined);
  await bare.page.waitForTimeout(2500);
  const listed = await bare.page.getByText('e2e-packaged').count();
  check('without the bridge the page cannot list flows (it is not just working by luck)', listed === 0);
  await bare.context.close();
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id))) await api('POST', `/api/v1/flows/${f.id}/delete`);
  await browser.close();
  server.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
