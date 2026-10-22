import { writeFile } from 'node:fs/promises';
import { loadLocalEnvironment, SWITCH_CREDENTIALS_FILE, switchBridgeUrl } from './config.ts';
import { registerSwitchAgent } from './switchClient.ts';

loadLocalEnvironment();

const registrationToken = process.env.SWITCH_REGISTRATION_TOKEN?.trim();
if (!registrationToken) throw new Error('SWITCH_REGISTRATION_TOKEN is required to register Aqua with Switch.');

const bridgeUrl = switchBridgeUrl();
const registration = await registerSwitchAgent(bridgeUrl, registrationToken, {
  name: process.env.SWITCH_AGENT_NAME?.trim() || 'aqua-company-brain',
  displayName: process.env.SWITCH_AGENT_DISPLAY_NAME?.trim() || 'Aqua Company Brain',
  description: 'Graph-grounded incident context, ownership, decisions, and team handoffs from Aqua.',
  connectorType: 'aqua-company-brain',
  overwrite: process.env.SWITCH_AGENT_OVERWRITE === 'true',
});

await writeFile(SWITCH_CREDENTIALS_FILE, `${JSON.stringify({
  bridgeUrl,
  agentId: registration.id,
  apiKey: registration.apiKey,
}, null, 2)}\n`, { encoding: 'utf8', mode: 0o600 });

console.log(`Registered Aqua Company Brain as ${registration.id}.`);
console.log(`Credentials were written to ${SWITCH_CREDENTIALS_FILE}.`);
