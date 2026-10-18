import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const config = JSON.parse(
  readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'),
);

test('release windows load the bundled frontend instead of the Vite development server', () => {
  const windows = config.app.windows as Array<{ label: string; url?: string }>;
  const main = windows.find((window) => window.label === 'main');
  const overlay = windows.find((window) => window.label === 'voice-overlay');

  assert.equal(main?.url, 'index.html');
  assert.equal(overlay?.url, 'index.html?overlay=true');
  for (const window of windows) {
    assert.doesNotMatch(window.url ?? '', /^https?:\/\//i);
  }
});

test('the installed windows may invoke every Aqua desktop command they use', () => {
  const capability = JSON.parse(
    readFileSync(new URL('../src-tauri/capabilities/default.json', import.meta.url), 'utf8'),
  );
  const required = [
    'gateway-info',
    'open-external',
    'bring-to-front',
    'orb-menu',
    'orb-resize',
    'orb-place-state',
    'orb-place-set',
    'orb-place-move',
    'orb-place-forget',
    'orb-place-recheck',
    'orb-hit-regions',
    'update-status',
    'check-for-update',
    'install-update',
    'update-progress',
  ];

  assert.deepEqual(capability.windows, ['main', 'voice-overlay']);
  for (const command of required) {
    assert.ok(
      capability.permissions.includes(`allow-${command}`),
      `missing allow-${command} from the installed-window capability`,
    );
  }
});
