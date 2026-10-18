// Typed client for aqua-core's AI Flows (/api/v1/flows/*). The dev proxy adds the tokens. Reading,
// validating and diffing need any token; saving, restoring, importing and deleting need the UI token.

import { requireJson } from './jsonResponse.ts';

export type NodeKind =
  | 'trigger' | 'user_input' | 'research' | 'text_generation' | 'image_generation' | 'ai_agent' | 'browser' | 'website'
  | 'http' | 'n8n' | 'comment_replies' | 'tool' | 'file' | 'excel' | 'database' | 'social_media' | 'condition' | 'loop' | 'delay' | 'schedule'
  | 'approval' | 'notification' | 'set_variable';

export type Risk = 'low' | 'medium' | 'high';
export type FieldKind = 'text' | 'long_text' | 'number' | 'bool' | 'choice' | 'json' | 'url';

export interface FieldSpec {
  name: string;
  label: string;
  kind: FieldKind;
  required: boolean;
  choices: string[];
  default: unknown;
  help: string;
}

export interface NodeSpec {
  kind: NodeKind;
  label: string;
  description: string;
  category: string;
  risk: Risk;
  out_ports: string[];
  default_outputs: string[];
  fields: FieldSpec[];
  capabilities: string[];
  waits_for_person: boolean;
  /** Whether anything can run this kind of step. A core older than the flag does not send it. */
  runnable?: boolean;
}

export interface RetryPolicy {
  retries: number;
  delay_secs: number;
  backoff: 'fixed' | 'exponential';
}

export interface FlowNode {
  id: string;
  kind: NodeKind;
  name: string;
  description: string;
  config: Record<string, unknown>;
  inputs: string[];
  outputs: string[];
  position: { x: number; y: number };
  enabled: boolean;
  retry: RetryPolicy;
  on_error: 'fail' | 'continue' | 'ask_user';
  credential: string | null;
}

export interface FlowEdge {
  id: string;
  from: string;
  to: string;
  port: string;
}

export interface VariableDef {
  name: string;
  label: string;
  kind: 'text' | 'long_text' | 'number' | 'bool' | 'choice';
  required: boolean;
  default: unknown;
  choices: string[];
}

export interface FlowSettings {
  approval: 'always' | 'new_only' | 'never';
  timezone: string;
}

export interface FlowDef {
  name: string;
  description: string;
  nodes: FlowNode[];
  edges: FlowEdge[];
  variables: VariableDef[];
  settings: FlowSettings;
}

export interface Flow extends FlowDef {
  id: number;
  owner_id: string;
  version: number;
  created_at: number;
  updated_at: number;
}

export interface FlowSummary {
  id: number;
  name: string;
  description: string;
  version: number;
  node_count: number;
  updated_at: number;
}

export interface Issue {
  severity: 'error' | 'warning' | 'info';
  node: string | null;
  code: string;
  message: string;
}

export interface Report {
  issues: Issue[];
  needs_approval: string[];
}

export interface VersionInfo {
  version: number;
  author: string;
  note: string;
  created_at: number;
}

export interface Diff {
  nodes_added: string[];
  nodes_removed: string[];
  nodes_changed: { id: string; name: string; fields: string[] }[];
  nodes_moved: string[];
  edges_added: string[];
  edges_removed: string[];
  flow_changed: string[];
  summary: string[];
}

export interface AuditEntry {
  id: number;
  at: number;
  actor: string;
  action: string;
  detail: string;
}

async function json<T>(r: Response): Promise<T> {
  return requireJson<T>(r, `Could not load or update this routine (${r.status}).`);
}

const send = (method: string, url: string, body?: unknown) =>
  fetch(url, { method, headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });

export interface Question {
  id: string;
  question: string;
  choices: string[];
  blocking: boolean;
}

export interface Resolution {
  node: string;
  name: string;
  capability: string;
  mechanism: string;
  note: string;
}

/** A command is applied by the server; the client only carries it from the proposal to the apply call. */
export type Command = Record<string, unknown>;

export interface Proposal {
  kind: 'create' | 'edit';
  understood: boolean;
  title: string;
  explanation: string[];
  questions: Question[];
  warnings: string[];
  commands: Command[];
  base_version: number | null;
  result: FlowDef;
  diff: Diff;
  validation: Report;
  resolution: Resolution[];
  source: 'rules' | 'model' | 'rules+model';
  not_understood: string[];
  schedule?: { cron: string; summary: string; timezone: string; end_on?: string | null; max_runs?: number | null } | null;
}

export interface ChatMessage {
  id: number;
  at: number;
  role: 'user' | 'assistant';
  text: string;
}

export type RunStatus = 'queued' | 'running' | 'waiting' | 'paused' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted';
export type NodeStatus = 'pending' | 'running' | 'waiting' | 'succeeded' | 'failed' | 'skipped' | 'needs_action' | 'interrupted';

export type WaitRequest =
  | { kind: 'approval'; title: string; preview: string; allow_edit: boolean; implicit: boolean }
  | { kind: 'input'; question: string; answer_kind: string; options: string[] }
  | { kind: 'until'; ts: number }
  | { kind: 'failure'; message: string; choices: string[] };

export interface NodeRun {
  run_id: number;
  node_id: string;
  status: NodeStatus;
  attempts: number;
  started_at: number;
  finished_at: number;
  outputs: Record<string, unknown>;
  taken: string[];
  error: string;
  wait: WaitRequest | null;
  gate_approved: boolean;
  note: string;
}

export interface Run {
  id: number;
  flow_id: number;
  flow_version: number;
  name: string;
  status: RunStatus;
  trigger_kind: string;
  dry_run: boolean;
  vars: Record<string, unknown>;
  created_at: number;
  started_at: number;
  finished_at: number;
  error: string;
  from_run: number | null;
  def: FlowDef;
}

export interface RunEvent {
  id: number;
  run_id: number;
  at: number;
  node_id: string | null;
  level: 'info' | 'warn' | 'error' | 'notify';
  kind: string;
  message: string;
}

export interface InboxItem {
  run_id: number;
  flow_id: number;
  flow_name: string;
  node_id: string;
  node_name: string;
  since: number;
  wait: WaitRequest;
}

export interface RunResponse {
  decision?: 'approved' | 'rejected';
  answer?: string;
  edits?: string;
  choice?: string;
}


export type Misfire = 'run_now' | 'skip' | 'ask' | 'next_run';
export type ScheduleWhen = { type: 'cron'; cron: string } | { type: 'once'; when: string };
export interface NotifyPrefs { on_start: boolean; on_success: boolean; on_failure: boolean; on_recovered: boolean; on_needs_you: boolean; on_each_step: boolean; channels: string[] }
export interface Campaign { end_on?: string | null; max_runs?: number | null; series?: Record<string, unknown>[] }
export interface CampaignView { name: string; runs_started: number; total_planned: number | null; remaining: number; worked: number; recovered: number; failed: number; unverified: number; in_progress: number; skipped: number; awaiting_decision: number; next_run_at: number | null; ends_at: number | null; over: boolean; text: string }
export interface ScheduleBody { campaign?: Campaign; name?: string; text?: string; when?: ScheduleWhen; timezone?: string; inputs?: Record<string, unknown>; misfire?: Misfire; notify?: NotifyPrefs; enabled?: boolean }
export interface ScheduleView {
  id: number; flow_id: number; name: string; when: ScheduleWhen; timezone: string; inputs: Record<string, unknown>; misfire: Misfire; notify: NotifyPrefs;
  campaign: Campaign; runs_started: number; ends_at: number | null;
  enabled: boolean; next_run_at: number | null; last_run_at: number; last_run_id: number | null; last_status: string;
  state: 'paused' | 'scheduled' | 'running' | 'needs_attention' | 'completed' | 'invalid';
  flow_name: string; zone_label: string; next_run_text: string; active_run: number | null; pending_missed: number | null;
}
export interface Occurrence { id: number; schedule_id: number; due_at: number; decided_at: number; outcome: string; run_id: number | null; reason: string }
export interface Notification { id: number; at: number; kind: string; level: 'info' | 'success' | 'warning' | 'error'; flow_id: number; run_id: number | null; schedule_id: number | null; title: string; body: string; read: boolean; delivery: { channel: string; ok: boolean; note: string }[] }

export interface Knowledge { fix_key: string; class: string; successes: number; failures: number; confidence: number; state: 'candidate' | 'trusted' | 'retired' }
export interface Suggestion { key: string; kind: 'temporary' | 'permanent'; title: string; why: string; changes: string[]; changes_logic: boolean; needs_approval: boolean; applicable: boolean; knowledge: Knowledge }
export interface RecoveryStep { node_id: string; node_name: string; error: string; class: string; label: string; explain: string; guidance: string | null; suggestions: Suggestion[] }
export interface Verdict { kind: 'success' | 'recovered' | 'failed' | 'unknown' | 'waiting'; reason: string; evidence: string[]; class: string | null }
export interface RecoveryRecord { id: number; flow_id: number; run_id: number; node_id: string; class: string; fix_key: string; title: string; kind: string; version_before: number; version_after: number; status: 'applied' | 'rolled_back'; outcome: 'pending' | 'worked' | 'did_not_help'; created_at: number }

export interface Health { flow_id: number; name: string; state: 'idle' | 'healthy' | 'degraded' | 'failing'; score: number | null; runs: number; succeeded: number; recovered: number; failed: number; unverified: number; last_run_at: number; last_success_at: number; failure_streak: number; top_problem: [string, number] | null; reasons: string[] }
export interface DayBucket { day: number; starts_at: number; success: number; recovered: number; failed: number; unverified: number }
export interface ErrorGroup { class: string; label: string; explain: string; count: number; flows: string[]; last_at: number; example: string; last_run_id: number; last_flow_id: number }
export interface AskProposal { run_id: number; flow_id: number; flow_name: string; node_id: string; key: string; title: string; changes_logic: boolean }
export interface AskReply { understood: boolean; reply: string; proposals: AskProposal[]; did: string[] }

export interface N8nHealth { configured: boolean; reachable: boolean; base_url: string | null; detail: string }

export interface CommentDraft {
  id: number;
  flow_id: number;
  run_id: number;
  post_url: string;
  comment_id: string;
  author: string;
  comment_text: string;
  category: string;
  reply: string;
  note: string;
  sent_text: string;
  status: 'pending' | 'posting' | 'posted' | 'unknown' | 'failed' | 'skipped';
  error: string;
  created_at: number;
  decided_at: number;
}

export const flowsApi = {
  commentDrafts: (status?: string) =>
    fetch(`/api/v1/flows/comments/drafts${status ? `?status=${status}` : ''}`).then((r) => json<{ drafts: CommentDraft[]; counts: Record<string, number> }>(r)),
  approveComment: (id: number, text: string) => send('POST', `/api/v1/flows/comments/drafts/${id}/approve`, { text }).then((r) => json<{ draft: CommentDraft }>(r)),
  skipComment: (id: number) => send('POST', `/api/v1/flows/comments/drafts/${id}/skip`).then((r) => json<{ draft: CommentDraft }>(r)),
  resolveComment: (id: number, action: 'posted' | 'retry') => send('POST', `/api/v1/flows/comments/drafts/${id}/resolve`, { action }).then((r) => json<{ draft: CommentDraft }>(r)),
  list: () => fetch('/api/v1/flows').then((r) => json<{ flows: FlowSummary[] }>(r)),
  nodeTypes: () => fetch('/api/v1/flows/node-types').then((r) => json<{ node_types: NodeSpec[]; platforms: string[]; platform_actions: string[] }>(r)),
  templates: () => fetch('/api/v1/flows/templates').then((r) => json<{ templates: { id: string; name: string; description: string }[] }>(r)),
  create: (body: { template: string; name?: string } | FlowDef) => send('POST', '/api/v1/flows', body).then((r) => json<Flow>(r)),
  get: (id: number) => fetch(`/api/v1/flows/${id}`).then((r) => json<Flow>(r)),
  save: (id: number, def: FlowDef, expectedVersion: number, note = '') => send('PUT', `/api/v1/flows/${id}`, { def, expected_version: expectedVersion, note }).then((r) => json<Flow>(r)),
  remove: (id: number) => send('POST', `/api/v1/flows/${id}/delete`).then((r) => json<{ deleted: boolean }>(r)),
  validate: (def: FlowDef) => send('POST', '/api/v1/flows/validate', def).then((r) => json<Report>(r)),
  versions: (id: number) => fetch(`/api/v1/flows/${id}/versions`).then((r) => json<{ versions: VersionInfo[] }>(r)),
  version: (id: number, v: number) => fetch(`/api/v1/flows/${id}/versions/${v}`).then((r) => json<FlowDef>(r)),
  restore: (id: number, v: number) => send('POST', `/api/v1/flows/${id}/restore/${v}`).then((r) => json<Flow>(r)),
  diff: (id: number, from: number, to: number) => fetch(`/api/v1/flows/${id}/diff?from=${from}&to=${to}`).then((r) => json<Diff>(r)),
  audit: (id: number) => fetch(`/api/v1/flows/${id}/audit?limit=30`).then((r) => json<{ audit: AuditEntry[] }>(r)),
  plan: (request: string, flowId?: number, answers?: Record<string, string>) => send('POST', '/api/v1/flows/plan', { request, flow_id: flowId, answers: answers ?? {} }).then((r) => json<Proposal>(r)),
  apply: (p: Proposal, flowId?: number) =>
    send('POST', '/api/v1/flows/plan/apply', { flow_id: flowId, expected_version: p.base_version ?? undefined, commands: p.commands, title: p.title, schedule: p.schedule ?? undefined }).then((r) => json<{ flow: Flow; previous_version: number | null; notes: string[]; schedule?: { id: number } | null; schedule_note?: string | null }>(r)),
  chat: (id: number) => fetch(`/api/v1/flows/${id}/chat`).then((r) => json<{ messages: ChatMessage[] }>(r)),
  startRun: (id: number, inputs: Record<string, unknown>, dryRun: boolean) => send('POST', `/api/v1/flows/${id}/run`, { inputs, dry_run: dryRun }).then((r) => json<{ run: Run }>(r)),
  runs: (id: number, limit = 20) => fetch(`/api/v1/flows/${id}/runs?limit=${limit}`).then((r) => json<{ runs: Run[] }>(r)),
  runDetail: (rid: number) => fetch(`/api/v1/flows/runs/${rid}`).then((r) => json<{ run: Run; nodes: NodeRun[] }>(r)),
  runEvents: (rid: number, after = 0) => fetch(`/api/v1/flows/runs/${rid}/events?after=${after}&limit=500`).then((r) => json<{ events: RunEvent[] }>(r)),
  respond: (rid: number, node: string, r: RunResponse) => send('POST', `/api/v1/flows/runs/${rid}/respond`, { node, ...r }).then((x) => json<{ ok: boolean }>(x)),
  control: (rid: number, action: 'pause' | 'resume' | 'stop' | 'restart') => send('POST', `/api/v1/flows/runs/${rid}/${action}`).then((x) => json<{ ok?: boolean; run?: Run }>(x)),
  nodeAction: (rid: number, node: string, action: 'retry' | 'skip' | 'run-from') => send('POST', `/api/v1/flows/runs/${rid}/nodes/${encodeURIComponent(node)}/${action}`).then((x) => json<{ ok?: boolean; run?: Run }>(x)),
  schedules: (flowId?: number) => fetch(flowId === undefined ? '/api/v1/flows/schedules' : `/api/v1/flows/${flowId}/schedules`).then((r) => json<{ schedules: ScheduleView[] }>(r)),
  createSchedule: (flowId: number, b: ScheduleBody) => send('POST', `/api/v1/flows/${flowId}/schedules`, b).then((r) => json<{ schedule: ScheduleView; summary: string | null }>(r)),
  updateSchedule: (id: number, b: ScheduleBody) => send('PUT', `/api/v1/flows/schedules/${id}`, b).then((r) => json<{ schedule: ScheduleView }>(r)),
  deleteSchedule: (id: number) => send('POST', `/api/v1/flows/schedules/${id}/delete`).then((r) => json<{ deleted: boolean }>(r)),
  scheduleAction: (id: number, a: 'enable' | 'disable' | 'run-now') => send('POST', `/api/v1/flows/schedules/${id}/${a}`).then((r) => json<{ run?: Run }>(r)),
  previewSchedule: (b: ScheduleBody) => send('POST', '/api/v1/flows/schedules/preview', b).then((r) => json<{ summary: string | null; zone: string; next: { at: number; text: string }[] }>(r)),
  campaign: (id: number) => fetch(`/api/v1/flows/schedules/${id}/campaign`).then((r) => json<{ campaign: CampaignView }>(r)),
  occurrences: (id: number) => fetch(`/api/v1/flows/schedules/${id}/occurrences?limit=20`).then((r) => json<{ occurrences: Occurrence[] }>(r)),
  resolveMissed: (occurrenceId: number, decision: 'run' | 'skip') => send('POST', `/api/v1/flows/schedules/missed/${occurrenceId}/${decision}`).then((r) => json<{ run: Run | null }>(r)),
  notifications: (unreadOnly = false) => fetch(`/api/v1/flows/notifications?unread=${unreadOnly}&limit=40`).then((r) => json<{ notifications: Notification[]; unread: number }>(r)),
  markRead: (ids: number[]) => send('POST', '/api/v1/flows/notifications/read', { ids }).then((r) => json<{ marked: number }>(r)),
  runRecovery: (rid: number) => fetch(`/api/v1/flows/runs/${rid}/recovery`).then((r) => json<{ verdict: Verdict; steps: RecoveryStep[] }>(r)),
  applyRecovery: (rid: number, node: string, key: string) => send('POST', `/api/v1/flows/runs/${rid}/recovery/apply`, { node, key }).then((r) => json<{ version: number; recovery: RecoveryRecord }>(r)),
  rollbackRecovery: (id: number) => send('POST', `/api/v1/flows/recovery/${id}/rollback`).then((r) => json<{ version: number }>(r)),
  recoveries: (flowId: number) => fetch(`/api/v1/flows/${flowId}/recoveries`).then((r) => json<{ recoveries: RecoveryRecord[] }>(r)),
  health: () => fetch('/api/v1/flows/health').then((r) => json<{ flows: { health: Health; history: DayBucket[] }[]; errors: ErrorGroup[] }>(r)),
  askAssistant: (text: string) => send('POST', '/api/v1/flows/assistant', { text }).then((r) => json<AskReply>(r)),
  n8nHealth: () => fetch('/api/v1/flows/n8n/health').then((r) => json<N8nHealth>(r)),
  inbox: () => fetch('/api/v1/flows/inbox').then((r) => json<{ items: InboxItem[] }>(r)),
  artifactUrl: (rid: number, node: string, key = 'image') => `/api/v1/flows/runs/${rid}/artifact?node=${encodeURIComponent(node)}&key=${encodeURIComponent(key)}`,
  streamUrl: (rid: number, after = 0) => `/api/v1/flows/runs/${rid}/stream?after=${after}`,
  importFlow: (def: FlowDef) => send('POST', '/api/v1/flows/import', def).then((r) => json<Flow>(r)),
};

export const NODE_W = 220;
export const NODE_H = 66;

/** Layered layout: a step goes one column-row below its deepest predecessor; siblings spread sideways. */
export function autoLayout(def: FlowDef): FlowDef {
  const depth = new Map<string, number>();
  const preds = new Map<string, string[]>();
  def.nodes.forEach((n) => preds.set(n.id, []));
  def.edges.forEach((e) => preds.get(e.to)?.push(e.from));
  const visit = (id: string, seen: Set<string>): number => {
    if (depth.has(id)) return depth.get(id)!;
    if (seen.has(id)) return 0; // a cycle: the validator reports it, layout just must not hang
    seen.add(id);
    const d = Math.max(-1, ...(preds.get(id) ?? []).map((p) => visit(p, seen))) + 1;
    depth.set(id, d);
    return d;
  };
  def.nodes.forEach((n) => visit(n.id, new Set()));
  const rows = new Map<number, string[]>();
  def.nodes.forEach((n) => {
    const d = depth.get(n.id) ?? 0;
    rows.set(d, [...(rows.get(d) ?? []), n.id]);
  });
  const nodes = def.nodes.map((n) => {
    const d = depth.get(n.id) ?? 0;
    const i = rows.get(d)!.indexOf(n.id);
    return { ...n, position: { x: 60 + i * (NODE_W + 60), y: 40 + d * (NODE_H + 60) } };
  });
  return { ...def, nodes };
}

export const RISK_COLOUR: Record<Risk, string> = { low: '#10b981', medium: '#f59e0b', high: '#f87171' };
