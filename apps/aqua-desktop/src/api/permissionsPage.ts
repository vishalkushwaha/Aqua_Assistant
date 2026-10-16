// What the Permissions page puts together besides the cards that already exist (full access, the browser): the project folders whose
// notes file Aqua may follow (GET /api/v1/projects, POST /api/v1/projects/trust), the programs the person added in mcp.json
// (GET /api/v1/tools/mcp/servers, POST .../{id}/trust and .../{id}/untrust) and the last calls of the tool host
// (GET /api/v1/tools/audit/recent). All of them need the UI token, which the shell or the dev proxy adds.
//
// (src/api/permissions.ts is the client of full access and stays as it is.) Everything here is a plain function, so a test can call it
// without a page. Text from the core (a folder, a command line, a tool, a reason) is only ever handed to React as a text child.

import { agoText, AquaError, bodyOrThrow, callCore, clip, isObject, jsonInit, positiveOrNull, stringList, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

// ---- trusted project folders -----------------------------------------------------------------------------------------------------

export interface TrustedFolder {
  /** As the core stores it (lower case, forward slashes). */
  folder: string;
  /** The notes file (AQUA.md or AGENTS.md) is in the folder now. */
  filePresent: boolean;
  /** The file is still the text that was trusted. */
  unchanged: boolean;
}

export function mapFolder(raw: unknown): TrustedFolder | null {
  if (!isObject(raw) || typeof raw.folder !== 'string' || raw.folder.trim() === '') return null;
  // A field the core does not send is "not known to be fine": the folder is shown as one to look at.
  return { folder: raw.folder, filePresent: raw.file_present === true, unchanged: raw.unchanged === true };
}

/** The trusted folders (`trusted`, as the list route and the trust route's `list` both carry them), or null when it is not a list. */
export function mapFolders(list: unknown): TrustedFolder[] | null {
  if (!Array.isArray(list)) return null;
  const folders: TrustedFolder[] = [];
  for (const row of list) {
    const folder = mapFolder(row);
    if (folder) folders.push(folder);
  }
  return folders.sort((a, b) => a.folder.localeCompare(b.folder));
}

export type Tone = 'ok' | 'attention' | 'problem' | 'neutral';

/** What the standing of a trusted folder means, in a sentence, and how loudly to say it. */
export function folderStanding(folder: TrustedFolder): { text: string; tone: Tone; canTrustAgain: boolean } {
  if (!folder.filePresent) return { text: 'There is no notes file in this folder now, so there is nothing for Aqua to follow.', tone: 'attention', canTrustAgain: false };
  if (folder.unchanged) return { text: 'Aqua follows the notes file as it was when you trusted it.', tone: 'ok', canTrustAgain: false };
  return { text: 'The notes file has changed since you trusted it, so Aqua ignores it until you have read it and trusted it again.', tone: 'attention', canTrustAgain: true };
}

export const NOTES_FILE_HINT = 'a notes file called AQUA.md or AGENTS.md';

// ---- MCP servers the person added --------------------------------------------------------------------------------------------------

export interface McpEnv {
  name: string;
  /** A literal value, shown whole by the core; null for a secret. */
  value: string | null;
  /** The name of a secret the value comes from (never the value); null for a literal. */
  secret: string | null;
}

export interface McpTool {
  name: string;
  description: string;
  readOnly: boolean;
}

export type McpState = 'not_trusted' | 'starting' | 'ready' | 'failed' | 'unknown';

export interface McpServer {
  id: string;
  /** The exact command line the server is started with. */
  commandLine: string;
  cwd: string | null;
  env: McpEnv[];
  toolsAllow: string[];
  readOnly: string[];
  trusted: boolean;
  state: McpState;
  /** Why a server could not be used, in the core's words; '' otherwise. */
  why: string;
  tools: McpTool[];
}

export interface McpList {
  servers: McpServer[];
  /** The core could read the list but not mcp.json (a sentence of the core's), or ''. */
  problem: string;
}

const MCP_STATES: readonly McpState[] = ['not_trusted', 'starting', 'ready', 'failed'];
const MAX_ID_CHARS = 200;

function mapEnv(raw: unknown): McpEnv | null {
  if (!isObject(raw) || typeof raw.name !== 'string' || raw.name === '') return null;
  return { name: raw.name, value: typeof raw.value === 'string' ? raw.value : null, secret: typeof raw.secret === 'string' ? raw.secret : null };
}

function mapTool(raw: unknown): McpTool | null {
  if (!isObject(raw) || typeof raw.name !== 'string' || raw.name === '') return null;
  return { name: raw.name, description: text(raw.description), readOnly: raw.read_only === true };
}

function mapAll<T>(value: unknown, map: (raw: unknown) => T | null): T[] {
  const out: T[] = [];
  if (!Array.isArray(value)) return out;
  for (const row of value) {
    const mapped = map(row);
    if (mapped) out.push(mapped);
  }
  return out;
}

/** One server, or null when it has no usable id (it could be neither trusted nor untrusted). Trust is only ever "yes" when the core says so. */
export function mapServer(raw: unknown): McpServer | null {
  if (!isObject(raw) || typeof raw.id !== 'string') return null;
  const id = raw.id.trim();
  if (id === '' || id.length > MAX_ID_CHARS) return null;
  const state = MCP_STATES.find((s) => s === raw.state) ?? 'unknown';
  return {
    id,
    commandLine: text(raw.command_line),
    cwd: typeof raw.cwd === 'string' && raw.cwd !== '' ? raw.cwd : null,
    env: mapAll(raw.env, mapEnv),
    toolsAllow: stringList(raw.tools_allow),
    readOnly: stringList(raw.read_only),
    trusted: raw.trusted === true,
    state,
    why: text(raw.why),
    tools: mapAll(raw.tools, mapTool),
  };
}

/** The answer of the list route, or null when it is not a list of servers at all. */
export function mapServers(body: unknown): McpList | null {
  if (!isObject(body) || !Array.isArray(body.servers)) return null;
  return { servers: mapAll(body.servers, mapServer), problem: clip(text(body.error).trim(), 300) };
}

export function serverStanding(server: McpServer): { text: string; tone: Tone } {
  switch (server.state) {
    case 'not_trusted': return { text: 'Not trusted: nothing of it is started.', tone: 'neutral' };
    case 'starting': return { text: 'Starting…', tone: 'attention' };
    case 'ready': return { text: server.tools.length === 1 ? 'Running, with 1 tool.' : `Running, with ${server.tools.length} tools.`, tone: 'ok' };
    case 'failed': return { text: server.why === '' ? 'Trusted, but it could not be used.' : `Trusted, but it could not be used: ${clip(server.why, 240)}`, tone: 'problem' };
    default: return { text: server.trusted ? 'Trusted.' : 'Not trusted.', tone: server.trusted ? 'ok' : 'neutral' };
  }
}

/** Whether the "Trust" button is offered: a server that is not trusted, or one trusted whose tools changed (it failed) and has to be accepted again. */
export function canTrust(server: McpServer): boolean {
  return !server.trusted || server.state === 'failed';
}

/** Whether the "Stop trusting" button is offered. */
export function canUntrust(server: McpServer): boolean {
  return server.trusted;
}

/** `NAME = value` for a literal, `NAME = (secret: NAME2)` for a secret: the value of a secret is never in the answer. */
export function envText(env: McpEnv): string {
  if (env.secret !== null) return `${env.name} = (secret called ${env.secret})`;
  return `${env.name} = ${env.value ?? ''}`;
}

export function trustQuestion(server: McpServer): string {
  return `Trust "${server.id}"? Aqua will start this program on your computer exactly as written above, and add its tools.`;
}

export function untrustQuestion(server: McpServer): string {
  return `Stop trusting "${server.id}"? Aqua stops it, refuses its calls and takes its tools away.`;
}

// ---- the last calls of the tool host ---------------------------------------------------------------------------------------------

export interface AuditRow {
  seq: number | null;
  atMs: number | null;
  tool: string;
  caller: string;
  decision: string;
  ok: boolean;
  errorCode: string | null;
}

export function mapAuditRow(raw: unknown): AuditRow | null {
  if (!isObject(raw)) return null;
  return {
    seq: typeof raw.seq === 'number' && Number.isFinite(raw.seq) ? raw.seq : null,
    atMs: positiveOrNull(raw.ts_unix_ms),
    tool: text(raw.tool),
    caller: text(raw.caller),
    decision: text(raw.decision),
    ok: raw.ok === true,
    errorCode: typeof raw.error_code === 'string' && raw.error_code !== '' ? raw.error_code : null,
  };
}

/** The calls, newest first (the core sends them that way; the order is also kept by the sequence number when there is one). */
export function mapAudit(body: unknown): AuditRow[] | null {
  if (!isObject(body) || !Array.isArray(body.records)) return null;
  return mapAll(body.records, mapAuditRow).sort((a, b) => (b.seq ?? Number.NEGATIVE_INFINITY) - (a.seq ?? Number.NEGATIVE_INFINITY) || 0);
}

/** Who asked for the call, in words. A caller the app does not know is shown as the core wrote it. */
export function callerText(caller: string): string {
  switch (caller) {
    case 'agent': return 'the agent';
    case 'ui': return 'you, in the app';
    case 'voice': return 'your voice';
    case 'api': return 'a script (API)';
    case 'mcp_client': return 'an outside MCP client';
    case 'replay': return 'a saved recipe';
    case '': return 'not recorded';
    default: return clip(caller, 40);
  }
}

const REFUSED = ['denied_by_policy', 'denied_by_user', 'approval_timeout', 'invalid', 'unknown_tool', 'unavailable'];

/** What was decided, in words. A decision the app does not know is shown as the core wrote it, with spaces for underscores. */
export function decisionText(decision: string): string {
  switch (decision) {
    case 'auto': return 'allowed by the policy';
    case 'approved': return 'you approved it';
    case 'approved_always': return 'you approved it, and said always';
    case 'approved_session': return 'you approved it for the session';
    case 'denied_by_policy': return 'refused by the policy';
    case 'denied_by_user': return 'you said no';
    case 'approval_timeout': return 'nobody answered, so it was refused';
    case 'invalid': return 'refused: not a valid call';
    case 'unknown_tool': return 'refused: no such tool';
    case 'unavailable': return 'refused: the tool is not available';
    case '': return 'not recorded';
    default: return clip(decision.replace(/_/g, ' '), 60);
  }
}

/** What became of the call: it did not run when it was refused. */
export function resultText(row: AuditRow): string {
  if (REFUSED.includes(row.decision)) return 'did not run';
  if (row.ok) return 'ok';
  if (row.errorCode === 'STARTED') return 'started';
  return row.errorCode === null ? 'failed' : `failed (${clip(row.errorCode, 40)})`;
}

export function whenText(row: AuditRow, nowSecs: number): string {
  return row.atMs === null ? '' : agoText(row.atMs / 1000, nowSecs);
}

// ---- the calls --------------------------------------------------------------------------------------------------------------------

const PROJECTS_WORDING: Wording = { fallback: 'The trusted folders could not be read.', status: { 404: 'This version of Aqua cannot list trusted folders.' } };
const TRUST_PROJECT_WORDING: Wording = { fallback: 'The trust for that folder could not be changed.' };
const SERVERS_WORDING: Wording = { fallback: 'The servers could not be read.', status: { 404: 'This version of Aqua cannot list MCP servers.' }, serverMax: 300 };
const SERVER_WORDING: Wording = { fallback: 'The server could not be changed.', serverMax: 300 };
const AUDIT_WORDING: Wording = { fallback: 'The last tool calls could not be read.', status: { 404: 'This version of Aqua cannot list the last tool calls.' } };

export const TYPE_A_FOLDER = 'Type the folder first.';
const MAX_FOLDER_CHARS = 500;

/** A server id goes into one path segment whole. */
function serverPath(id: string, action: 'trust' | 'untrust'): string {
  return `/api/v1/tools/mcp/servers/${encodeURIComponent(id)}/${action}`;
}

function serverId(id: string): string {
  const usable = id.trim();
  if (usable === '' || usable.length > MAX_ID_CHARS) throw new AquaError(SERVER_WORDING.fallback);
  return usable;
}

export const permissionsPageApi = {
  /** The folders whose notes file Aqua may follow (UI token). */
  projects: async (): Promise<TrustedFolder[]> => {
    const body = bodyOrThrow(await callCore('/api/v1/projects', undefined, PROJECTS_WORDING));
    const folders = isObject(body) ? mapFolders(body.trusted) : null;
    if (!folders) throw new AquaError(UNREADABLE_ANSWER);
    return folders;
  },
  /** Trusts the notes file of a folder as it is now (`trust` true), or withdraws the trust (false) (UI token). Nothing is sent for an empty folder. */
  trustProject: async (folder: string, trust: boolean): Promise<void> => {
    const shown = folder.trim();
    if (shown === '' || shown.length > MAX_FOLDER_CHARS) throw new AquaError(TYPE_A_FOLDER);
    bodyOrThrow(await callCore('/api/v1/projects/trust', jsonInit('POST', { folder: shown, trust }), TRUST_PROJECT_WORDING));
  },
  /** The programs the person added in mcp.json (UI token). */
  servers: async (): Promise<McpList> => {
    const list = mapServers(bodyOrThrow(await callCore('/api/v1/tools/mcp/servers', undefined, SERVERS_WORDING)));
    if (!list) throw new AquaError(UNREADABLE_ANSWER);
    return list;
  },
  /** Trusts a server as it is written in mcp.json: it is started and its tools are added (UI token). */
  trustServer: async (id: string): Promise<void> => {
    bodyOrThrow(await callCore(serverPath(serverId(id), 'trust'), jsonInit('POST'), SERVER_WORDING));
  },
  /** Stops trusting a server: it is stopped, its calls are refused and its tools go (UI token). */
  untrustServer: async (id: string): Promise<void> => {
    bodyOrThrow(await callCore(serverPath(serverId(id), 'untrust'), jsonInit('POST'), SERVER_WORDING));
  },
  /** The last calls the tool host recorded, newest first (UI token). */
  recentCalls: async (limit = 20): Promise<AuditRow[]> => {
    const shown = Math.min(200, Math.max(1, Math.floor(limit)));
    const rows = mapAudit(bodyOrThrow(await callCore(`/api/v1/tools/audit/recent?limit=${shown}`, undefined, AUDIT_WORDING)));
    if (!rows) throw new AquaError(UNREADABLE_ANSWER);
    return rows;
  },
};
