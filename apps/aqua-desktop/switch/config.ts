import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { randomUUID } from 'node:crypto';

export const SWITCH_CREDENTIALS_FILE = resolve(process.cwd(), '.switch-agent.json');
export const SWITCH_STATE_FILE = resolve(process.cwd(), '.switch-state.json');

export interface StoredSwitchCredentials {
  bridgeUrl: string;
  agentId: string;
  apiKey: string;
}

export interface StoredSwitchState {
  connectionId: string;
  cursor: number | null;
  generation: number | null;
}

function applyDotEnv(path: string): void {
  if (!existsSync(path)) return;
  for (const line of readFileSync(path, 'utf8').split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const separator = trimmed.indexOf('=');
    if (separator < 1) continue;
    const name = trimmed.slice(0, separator).trim();
    if (!name || process.env[name] !== undefined) continue;
    let value = trimmed.slice(separator + 1).trim();
    if (value.length >= 2 && ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'")))) {
      value = value.slice(1, -1);
    }
    process.env[name] = value;
  }
}

export function loadLocalEnvironment(): void {
  applyDotEnv(resolve(process.cwd(), '.env'));
}

export function switchBridgeUrl(): string {
  const raw = process.env.SWITCH_BRIDGE_URL?.trim();
  if (!raw) throw new Error('SWITCH_BRIDGE_URL is required.');
  const url = new URL(raw);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('SWITCH_BRIDGE_URL must use http or https.');
  }
  return url.toString().replace(/\/$/, '');
}

export function loadSwitchCredentials(): StoredSwitchCredentials {
  const fromEnvironment = {
    bridgeUrl: process.env.SWITCH_BRIDGE_URL?.trim() || '',
    agentId: process.env.SWITCH_AGENT_ID?.trim() || '',
    apiKey: process.env.SWITCH_AGENT_API_KEY?.trim() || '',
  };
  if (fromEnvironment.bridgeUrl && fromEnvironment.agentId && fromEnvironment.apiKey) return fromEnvironment;

  if (!existsSync(SWITCH_CREDENTIALS_FILE)) {
    throw new Error('Switch credentials are missing. Run `npm run switch:register` or set SWITCH_AGENT_ID and SWITCH_AGENT_API_KEY.');
  }
  const stored = JSON.parse(readFileSync(SWITCH_CREDENTIALS_FILE, 'utf8')) as Partial<StoredSwitchCredentials>;
  const credentials = {
    bridgeUrl: fromEnvironment.bridgeUrl || stored.bridgeUrl || '',
    agentId: fromEnvironment.agentId || stored.agentId || '',
    apiKey: fromEnvironment.apiKey || stored.apiKey || '',
  };
  if (!credentials.bridgeUrl || !credentials.agentId || !credentials.apiKey) {
    throw new Error('The stored Switch credentials are incomplete. Register the agent again.');
  }
  return credentials;
}

export function switchRoomIds(): string[] {
  return (process.env.SWITCH_ROOM_IDS || '').split(',').map((room) => room.trim()).filter(Boolean);
}

export function loadSwitchState(): StoredSwitchState {
  if (existsSync(SWITCH_STATE_FILE)) {
    const stored = JSON.parse(readFileSync(SWITCH_STATE_FILE, 'utf8')) as Partial<StoredSwitchState>;
    if (typeof stored.connectionId === 'string' && stored.connectionId) {
      return {
        connectionId: stored.connectionId,
        cursor: typeof stored.cursor === 'number' ? stored.cursor : null,
        generation: typeof stored.generation === 'number' ? stored.generation : null,
      };
    }
  }
  return { connectionId: randomUUID(), cursor: null, generation: null };
}
