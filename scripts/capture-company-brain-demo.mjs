import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const chromePath = process.env.CHROME_PATH || 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe';
const baseUrl = process.env.DEMO_URL || 'http://127.0.0.1:1420';
const outputDir = resolve(process.argv[2] || 'demo/video/frames');
const scenes = [
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

console.log(`Captured ${scenes.length} live Aqua showcase frames in ${outputDir}`);
