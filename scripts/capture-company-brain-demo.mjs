import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const chromePath = process.env.CHROME_PATH || 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe';
const baseUrl = process.env.DEMO_URL || 'http://127.0.0.1:1420';
const outputDir = resolve(process.argv[2] || 'demo/video/frames');
const mode = process.argv[3] || 'showcase';
const showcaseScenes = [
  ['?showcase=overview', '01-overview.png', 8000],
  ['?demo=answer', '02-company-brain-answer.png', 22000],
  ['?demo=action', '03-company-brain-action.png', 28000],
  ['?demo=handoff', '04-company-brain-memory.png', 40000],
  ['?showcase=graph-memory', '05-memory-graph.png', 12000],
  ['?showcase=showcase-capabilities', '06-capabilities.png', 8000],
  ['?showcase=tts-voices', '07-voice-profiles.png', 10000],
  ['?showcase=security', '08-voice-security.png', 10000],
  ['?showcase=showcase-platform', '09-platform-support.png', 8000],
];
const submissionScenes = [
  ['?showcase=overview', 'submission-01-overview.png', 8000],
  ['?demo=seed', 'submission-02-dataset-ready.png', 14000],
  ['?demo=answer', 'submission-03-grounded-answer.png', 24000],
  ['?demo=action', 'submission-04-graph-action.png', 30000],
  ['?demo=memory', 'submission-05-persistent-memory.png', 36000],
  ['?demo=handoff', 'submission-06-agent-handoff.png', 42000],
  ['?showcase=graph-memory', 'submission-07-live-graph.png', 14000],
  ['?showcase=showcase-capabilities', 'submission-09-capabilities.png', 8000],
  ['?showcase=showcase-platform', 'submission-10-platform.png', 8000],
];
const failureScenes = [
  ['?demo=answer', 'submission-08-falkordb-refusal.png', 18000],
];
const completeFailureScenes = [
  ['?demo=answer', 'complete-12-falkordb-refusal.png', 18000],
];
const completeScenes = [
  ['?showcase=overview', 'complete-01-overview.png', 8000],
  ['?showcase=showcase-local-ai', 'complete-02-local-ai.png', 8000],
  ['?showcase=showcase-voice', 'complete-03-voice.png', 8000],
  ['?showcase=showcase-automation', 'complete-04-automation.png', 8000],
  ['?showcase=showcase-knowledge', 'complete-05-knowledge.png', 8000],
  ['?demo=seed', 'complete-06-company-graph.png', 14000],
  ['?demo=answer', 'complete-07-grounded-answer.png', 24000],
  ['?demo=action', 'complete-08-graph-action.png', 30000],
  ['?demo=memory', 'complete-09-persistent-memory.png', 36000],
  ['?demo=handoff', 'complete-10-agent-handoff.png', 42000],
  ['?showcase=graph-memory', 'complete-11-live-graph.png', 14000],
  ['?showcase=showcase-capabilities', 'complete-13-capabilities.png', 8000],
  ['?showcase=showcase-platform', 'complete-14-platform.png', 8000],
];
const panelTabs = [
  'overview', 'tts-voices', 'security', 'workflows', 'flows', 'tools', 'permissions', 'memory',
  'graph-memory', 'company-brain', 'agent-learning', 'skills-inbox', 'file-index', 'vault', 'devices', 'orb-position',
];
const panelScenes = panelTabs.map((tab, index) => [
  `?showcase=${tab}`,
  `panel-${String(index + 1).padStart(2, '0')}-${tab}.png`,
  6000,
]);
const focusedScenes = {
  answer: submissionScenes.filter(([, filename]) => filename === 'submission-03-grounded-answer.png'),
  action: submissionScenes.filter(([, filename]) => filename === 'submission-04-graph-action.png'),
  memory: submissionScenes.filter(([, filename]) => filename === 'submission-05-persistent-memory.png'),
  handoff: submissionScenes.filter(([, filename]) => filename === 'submission-06-agent-handoff.png'),
};
const scenes = mode === 'submission'
  ? submissionScenes
  : mode === 'complete'
    ? completeScenes
  : mode === 'failure'
    ? failureScenes
    : mode === 'panel'
      ? panelScenes
      : focusedScenes[mode] || showcaseScenes;

await mkdir(outputDir, { recursive: true });

function capture(query, filename, budget) {
  return new Promise(async (resolveCapture, rejectCapture) => {
    const profileDir = await mkdtemp(join(tmpdir(), 'aqua-demo-chrome-'));
    const screenshotPath = join(outputDir, filename);
    const child = spawn(chromePath, [
      '--headless=new',
      '--disable-gpu',
      '--hide-scrollbars',
      '--no-first-run',
      '--no-default-browser-check',
      '--window-size=1920,1080',
      `--user-data-dir=${profileDir}`,
      `--virtual-time-budget=${budget}`,
      `--screenshot=${screenshotPath}`,
      `${baseUrl}/${query}`,
    ], { stdio: ['ignore', 'ignore', 'pipe'] });
    let errorText = '';
    child.stderr.on('data', (chunk) => { errorText += chunk.toString(); });
    child.on('error', rejectCapture);
    child.on('exit', async (code) => {
      await rm(profileDir, { recursive: true, force: true }).catch(() => {});
      if (code === 0) resolveCapture();
      else rejectCapture(new Error(`Chrome capture ${query} failed (${code}): ${errorText}`));
    });
  });
}

for (const [query, filename, budget] of scenes) {
  await capture(query, filename, budget);
}

if (mode === 'submission' || mode === 'complete') {
  const origin = new URL(baseUrl).origin;
  const request = async (path, options = {}) => {
    const response = await fetch(`${baseUrl}${path}`, {
      ...options,
      headers: { Origin: origin, Accept: 'application/json', ...(options.headers || {}) },
    });
    if (!response.ok) throw new Error(`${path} failed with ${response.status}`);
    return response.json();
  };
  const status = await request('/api/falkordb/status');
  try {
    await request('/api/falkordb/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ port: 1 }),
    });
    const dependencyScenes = mode === 'complete' ? completeFailureScenes : failureScenes;
    for (const [query, filename, budget] of dependencyScenes) {
      await capture(query, filename, budget);
    }
  } finally {
    await request('/api/falkordb/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ port: status.port }),
    });
  }
}

const capturedCount = scenes.length + (mode === 'submission' || mode === 'complete' ? 1 : 0);
console.log(`Captured ${capturedCount} live Aqua ${mode} frames in ${outputDir}`);
