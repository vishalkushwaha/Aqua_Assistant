import { writeFileSync } from 'node:fs';
import {
  loadLocalEnvironment,
  loadSwitchCredentials,
  loadSwitchState,
  SWITCH_STATE_FILE,
  switchRoomIds,
} from './config.ts';
import { SwitchClient } from './switchClient.ts';
import { CompanyBrainSwitchWorkflow } from './companyBrainWorkflow.ts';
import { createSwitchCompanyBrainDatabase } from './falkorCompanyBrain.ts';

loadLocalEnvironment();

const credentials = loadSwitchCredentials();
const state = loadSwitchState();
const rooms = switchRoomIds();
if (rooms.length === 0) throw new Error('SWITCH_ROOM_IDS must contain at least one room id.');
const database = createSwitchCompanyBrainDatabase();
const workflow = new CompanyBrainSwitchWorkflow({
  repository: database.repository,
  tenantId: process.env.SWITCH_COMPANY_TENANT_ID?.trim(),
  agentName: process.env.SWITCH_AGENT_NAME?.trim(),
  incidentAgentName: process.env.SWITCH_INCIDENT_AGENT_NAME?.trim(),
});

const controller = new AbortController();
const client = new SwitchClient({
  ...credentials,
  connectionId: state.connectionId,
  initialCursor: state.cursor,
  initialGeneration: state.generation,
  roomIds: rooms,
  scope: rooms.length > 1 ? 'all' : 'single',
  filter: 'addressed',
  onCheckpoint: (checkpoint) => {
    writeFileSync(SWITCH_STATE_FILE, `${JSON.stringify(checkpoint, null, 2)}\n`, 'utf8');
  },
  onControlFrame: (frame) => {
    if (frame.type === 'gap' || frame.type === 'evicted' || frame.type === 'refused') {
      process.stderr.write(`${JSON.stringify({ type: 'switch_control', frame })}\n`);
    }
  },
});

writeFileSync(SWITCH_STATE_FILE, `${JSON.stringify(state, null, 2)}\n`, 'utf8');

for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.once(signal, () => {
    controller.abort();
    client.close();
    void database.close();
  });
}

console.log(`Aqua Company Brain is listening in ${rooms.length} Switch room${rooms.length === 1 ? '' : 's'}.`);
await client.run(async (event) => {
  await workflow.handle(event, client);
}, controller.signal);
await database.close();
