// What Aqua did, run by run (documentation/UI_CONTRACT.md section 5.2; every route here needs the UI token, which the shell or the dev proxy adds, so
// these are plain relative fetches):
//   GET  /api/v1/runs                     the list, newest first (`limit`, `status`, `resumable=1`)
//   GET  /api/v1/runs/{id}                one run with its calls
//   GET  /api/v1/runs/{id}/safety         what the run read, held and could do (the safety strip)
//   GET  /api/v1/runs/{id}/proof          the signed record of the run, and whether it checks out
//   POST /api/v1/runs/{id}/undo           puts back what the run changed in files (`dry_run` only says what would be put back)
//   POST /api/v1/runs/{id}/resume         goes on with a run that stopped
//
// Everything that maps or words an answer is a plain function, so a test can call it without a page. Text from the core (a goal, an argument, what a
// tool returned) is made visible (see `visibleText`) and only ever handed to React as a text child.
//
// One thing the contract page gets wrong: it says times are in seconds, but a run's `started_at` and `ended_at` are in milliseconds
// (core/src/agent_runs/mod.rs, `now_ms`). `toMs` reads a number either way.

import {
  asText,
  count,
  cutText,
  failure,
  failureSentence,
  isObject,
  jsonInit,
  PlainError,
  sendRequest,
  serverSentence,
  shownText,
  UNREADABLE_ANSWER,
  type Answer as HttpAnswer,
} from './plainErrors.ts';
import { mapWhyStopped, type WhyStopped } from './whyStopped.ts';

export type Tone = 'ok' | 'attention' | 'problem' | 'neutral';

// ---- time ---------------------------------------------------------------------------------------

/** A unix time as milliseconds. A number under 1e11 is seconds (1e11 seconds is the year 5138; 1e11 milliseconds is 1973), anything above is milliseconds. */
export function toMs(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) return null;
  return value < 100_000_000_000 ? Math.round(value * 1000) : Math.round(value);
}

const localDate = (ms: number) => new Date(ms).toLocaleDateString();

/** "just now", "5 min ago", "3 h ago", and from two days on the date. `nowMs` is given so the answer does not depend on the clock. */
export function agoText(ms: number, nowMs: number, formatDate: (ms: number) => string = localDate): string {
  const s = Math.max(0, (nowMs - ms) / 1000);
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 86_400 * 2) return `${Math.round(s / 3600)} h ago`;
  return formatDate(ms);
}

/** "under a second", "12 s", "3 min 5 s", "1 h 2 min". */
export function durationText(ms: number): string {
  if (!Number.isFinite(ms) || ms < 1000) return 'under a second';
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s} s`;
  if (s < 3600) {
    const rest = s % 60;
    return rest === 0 ? `${Math.floor(s / 60)} min` : `${Math.floor(s / 60)} min ${rest} s`;
  }
  return `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
}

// ---- the state of a run -------------------------------------------------------------------------

const STATE_WORDS: Record<string, string> = {
  running: 'Running',
  stalled: 'Quiet for a while',
  completed: 'Finished',
  failed: 'Failed',
  cancelled: 'Stopped',
  interrupted: 'Interrupted',
  waiting_user: 'Waiting for you',
  expired: 'Question not answered',
};

export function stateWord(state: string): string {
  return STATE_WORDS[state] ?? 'Unknown';
}

export function stateTone(state: string): Tone {
  if (state === 'completed') return 'ok';
  if (state === 'failed') return 'problem';
  if (state === 'running' || state === 'cancelled' || state === '') return 'neutral';
  return STATE_WORDS[state] ? 'attention' : 'neutral';
}

/** The run is not over: it goes on (running, quiet for a while) or waits for the person. Its detail is read again; undo has to wait. */
export function isOpenState(state: string): boolean {
  return state === 'running' || state === 'stalled' || state === 'waiting_user';
}

/** The run is working now (not waiting for the person). */
export function isGoingState(state: string): boolean {
  return state === 'running' || state === 'stalled';
}

const REASON_WORDS: Record<string, string> = {
  final: 'It finished and gave its answer.',
  cancelled: 'It was stopped.',
  client_disconnected: 'It was stopped because the app that asked hung up.',
  user_declined: 'You said no to a question, so it stopped.',
  policy_denied: 'A rule did not allow a step, so it stopped.',
  needs_login: 'A page asked for a sign-in, so it stopped.',
  waiting_user: 'It is waiting for your answer.',
  process_ended: 'Aqua closed while it was working.',
  no_longer_running: 'It was no longer running when Aqua looked.',
  aborted: 'It ended unexpectedly.',
  question_expired: 'A question was not answered in time.',
  max_rounds: 'It used all the rounds it was allowed.',
  unparseable_reply: "The model's reply could not be read.",
};

/** Why a run ended, in a sentence. A reason the page does not know is shown as the core wrote it, with spaces for underscores. */
export function stoppedReasonText(reason: string): string {
  if (reason === '') return '';
  if (REASON_WORDS[reason]) return REASON_WORDS[reason];
  if (reason.startsWith('llm_error')) return 'The language model failed.';
  const plain = reason.replace(/_/g, ' ');
  return `Ended: ${plain}.`;
}

// ---- one run ------------------------------------------------------------------------------------

export interface RunSummary {
  id: string;
  /** The goal as stored: personal details and secrets already masked by the core. */
  goal: string;
  /** `running`, `completed`, `failed`, `cancelled`, `interrupted`, `waiting_user`, `expired`, `stalled`, or `unknown`. */
  state: string;
  startedMs: number | null;
  endedMs: number | null;
  caller: string;
  route: string;
  model: string;
  rounds: number;
  toolCalls: number;
  stoppedReason: string;
  finalAnswer: string;
  parentRunId: string | null;
}

/** One row of the list, or null when it has no id (the page could not open it). A missing field becomes the plain default. */
export function mapRun(raw: unknown): RunSummary | null {
  if (!isObject(raw)) return null;
  const id = raw.id;
  if (typeof id !== 'string' || id.trim() === '') return null;
  const parent = asText(raw.parent_run_id).trim();
  return {
    id,
    goal: shownText(raw.goal),
    state: typeof raw.status === 'string' && raw.status !== '' ? raw.status : 'unknown',
    startedMs: toMs(raw.started_at),
    endedMs: toMs(raw.ended_at),
    caller: shownText(raw.caller),
    route: shownText(raw.route),
    model: shownText(raw.model),
    rounds: count(raw.rounds),
    toolCalls: count(raw.tool_calls),
    stoppedReason: asText(raw.stopped_reason),
    finalAnswer: shownText(raw.final_answer, true),
    parentRunId: parent !== '' ? parent : null,
  };
}

/** Newest first; among runs that started together, the larger id first (as the core orders them); a run with no start time last. */
export function newestFirst(runs: RunSummary[]): RunSummary[] {
  return [...runs].sort((a, b) => (b.startedMs ?? 0) - (a.startedMs ?? 0) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
}

/** The list route's answer, newest first, or null when it is not a list at all. A row without an id is left out. */
export function mapRunList(body: unknown): RunSummary[] | null {
  if (!isObject(body) || !Array.isArray(body.runs)) return null;
  const runs: RunSummary[] = [];
  for (const row of body.runs) {
    const run = mapRun(row);
    if (run) runs.push(run);
  }
  return newestFirst(runs);
}

const MAX_GOAL_CHARS = 160;

/** The goal for a list row: cut when very long, and a stand-in when the core sent none. */
export function goalText(goal: string): string {
  const text = goal.trim();
  if (text === '') return '(no goal recorded)';
  return cutText(text, MAX_GOAL_CHARS);
}

export function callsText(n: number): string {
  return n === 1 ? '1 call' : `${n} calls`;
}

/** How long a run took or has been going: from its start to its end, or to `nowMs` while it goes on; null when the times are not known. */
export function runDurationMs(run: Pick<RunSummary, 'startedMs' | 'endedMs' | 'state'>, nowMs: number): number | null {
  if (run.startedMs === null) return null;
  const end = run.endedMs ?? (isGoingState(run.state) ? nowMs : null);
  if (end === null || end < run.startedMs) return null;
  return end - run.startedMs;
}

/** The one line under a goal: state, when, how long and how many calls. */
export function runFactsText(run: RunSummary, nowMs: number, formatDate?: (ms: number) => string): string {
  const parts: string[] = [];
  if (run.startedMs !== null) parts.push(agoText(run.startedMs, nowMs, formatDate));
  const took = runDurationMs(run, nowMs);
  if (took !== null) parts.push(isGoingState(run.state) ? `going for ${durationText(took)}` : `took ${durationText(took)}`);
  parts.push(callsText(run.toolCalls));
  return parts.join(' · ');
}

// ---- the calls of a run -------------------------------------------------------------------------

export interface RunCall {
  seq: number;
  tool: string;
  /** The arguments as the core redacted them, on one line. */
  arguments: string;
  /** What the call returned (the first part of it), with its line breaks. Text from outside: shown as data. */
  observation: string;
  status: string;
  ok: boolean | null;
  decision: string;
  approval: string;
  ms: number | null;
  atMs: number | null;
}

const MAX_ARGUMENT_CHARS = 400;
const MAX_OBSERVATION_CHARS = 600;

function argumentsText(value: unknown): string {
  let text = '';
  if (typeof value === 'string') text = value;
  else if (value !== null && value !== undefined) {
    try {
      text = JSON.stringify(value) ?? '';
    } catch {
      text = '';
    }
  }
  return cutText(shownText(text), MAX_ARGUMENT_CHARS);
}

/** A tool call of the run, or null when the step is not one (a plan, an answer) or not a step at all. */
export function mapCall(raw: unknown, index: number): RunCall | null {
  if (!isObject(raw) || raw.kind !== 'tool') return null;
  return {
    seq: typeof raw.seq === 'number' && Number.isFinite(raw.seq) ? raw.seq : index,
    tool: shownText(raw.tool),
    arguments: argumentsText(raw.arguments),
    observation: cutText(shownText(raw.observation, true), MAX_OBSERVATION_CHARS),
    status: asText(raw.status),
    ok: typeof raw.ok === 'boolean' ? raw.ok : null,
    decision: asText(raw.decision),
    approval: asText(raw.approval),
    ms: typeof raw.ms === 'number' && Number.isFinite(raw.ms) && raw.ms >= 0 ? Math.round(raw.ms) : null,
    atMs: toMs(raw.ts),
  };
}

/** How a call ended, in words (never by colour alone). `runGoing`: the run is working now, so a call with no result yet is the one in progress. */
export function callOutcome(call: RunCall, runGoing = false): { word: string; tone: Tone } {
  if (call.decision === 'denied') return { word: 'Refused', tone: 'problem' };
  if (call.status === 'interrupted') return { word: 'Cut off: it may or may not have happened', tone: 'attention' };
  if (call.status === 'started') return runGoing ? { word: 'Running now', tone: 'neutral' } : { word: 'No result recorded', tone: 'attention' };
  if (call.ok === false || call.status === 'failed') return { word: 'Failed', tone: 'problem' };
  if (call.ok === true || call.status === 'completed') return { word: 'Worked', tone: 'ok' };
  return { word: 'Unknown', tone: 'neutral' };
}

const APPROVAL_WORDS: Record<string, string> = {
  '': 'No question was asked',
  approved: 'You said yes',
  approved_session: 'A yes you gave earlier in the session covered it',
  approved_always: 'You said yes, and to remember it',
  denied: 'You said no',
  timeout: 'Nobody answered in time',
  cancelled: 'The question was taken back',
};

/** What became of the question about a call, in words. */
export function approvalWords(approval: string): string {
  return APPROVAL_WORDS[approval] ?? `Question: ${approval.replace(/_/g, ' ')}`;
}

export interface RunDetail extends RunSummary {
  tokensIn: number;
  tokensOut: number;
  /** The tool calls, in the order they were made. */
  calls: RunCall[];
  /** Why the run stopped, in the core's words (documentation/TRIAGE.md); null for a run that went well, one from before this existed, or one still going. */
  why: WhyStopped | null;
}

/** One run with its calls, or null when the answer is not a run. The steps that are not tool calls (plans, the answer) are left out. */
export function mapRunDetail(body: unknown): RunDetail | null {
  const run = mapRun(body);
  if (!run || !isObject(body)) return null;
  const steps = Array.isArray(body.steps) ? body.steps : [];
  const calls: RunCall[] = [];
  steps.forEach((s, i) => {
    const call = mapCall(s, i);
    if (call) calls.push(call);
  });
  calls.sort((a, b) => a.seq - b.seq);
  return { ...run, tokensIn: count(body.tokens_in), tokensOut: count(body.tokens_out), calls, why: mapWhyStopped(body) };
}

// ---- the safety strip ---------------------------------------------------------------------------

export type Mark = 'yes' | 'no' | 'unknown';

export interface SafetyProperty {
  mark: Mark;
  /** When the run came to hold it: "after 2 calls had come back", or '' when not said. */
  when: string;
  /** For the two that are about tools: which tools of the run it is about. */
  tools: string[];
}

export interface RunSafety {
  /** The run read text from outside (a page, a file, a window): text that can try to steer it. */
  readText: SafetyProperty;
  /** The run held data of the person's own. */
  yourData: SafetyProperty;
  canChange: SafetyProperty;
  canSend: SafetyProperty;
  /** A way to change something or to send something out: yes when either is, no when both are not. */
  reach: Mark;
  /** All three: text from outside, the person's data, and a way out. */
  allThree: boolean;
  /** The core's own sentence or two. */
  summary: string;
  /** The one page the core writes, to show somebody. */
  page: string;
}

export function whenText(afterCall: number): string {
  if (afterCall <= 0) return 'before any call had come back';
  return afterCall === 1 ? 'after 1 call had come back' : `after ${afterCall} calls had come back`;
}

function mapProperty(raw: unknown): SafetyProperty {
  if (!isObject(raw)) return { mark: 'unknown', when: '', tools: [] };
  const mark: Mark = raw.held === true ? 'yes' : raw.held === false ? 'no' : 'unknown';
  const after = raw.after_call;
  const tools = Array.isArray(raw.tools) ? raw.tools.filter((t): t is string => typeof t === 'string').slice(0, 20).map((t) => shownText(t)) : [];
  return { mark, when: mark === 'yes' && typeof after === 'number' && Number.isFinite(after) && after >= 0 ? whenText(Math.floor(after)) : '', tools };
}

function reachOf(change: Mark, send: Mark): Mark {
  if (change === 'yes' || send === 'yes') return 'yes';
  if (change === 'no' && send === 'no') return 'no';
  return 'unknown';
}

/**
 * The safety strip from `{ "safety": {...}, "page": "..." }`, or null when the answer is not that. A part the core does not say is `unknown`, never
 * `no`: a missing field must not read as reassurance.
 */
export function mapSafety(body: unknown): RunSafety | null {
  if (!isObject(body) || !isObject(body.safety)) return null;
  const s = body.safety;
  const readText = mapProperty(s.untrusted_input);
  const yourData = mapProperty(s.private_data);
  const canChange = mapProperty(s.can_change);
  const canSend = mapProperty(s.can_send);
  const reach = reachOf(canChange.mark, canSend.mark);
  return {
    readText,
    yourData,
    canChange,
    canSend,
    reach,
    allThree: readText.mark === 'yes' && yourData.mark === 'yes' && reach === 'yes',
    summary: shownText(s.summary),
    page: shownText(body.page, true),
  };
}

export type MarkKind = 'readText' | 'yourData' | 'reach';

export const MARK_TITLES: Record<MarkKind, string> = {
  readText: 'Read text from outside',
  yourData: 'Held data of yours',
  reach: 'Could change or send things',
};

const MARK_SENTENCES: Record<MarkKind, Record<Mark, string>> = {
  readText: {
    yes: 'Yes. It read text from outside (a web page, a file or a window). Such text can try to steer Aqua.',
    no: 'No. It read nothing from outside.',
    unknown: 'Aqua did not say.',
  },
  yourData: {
    yes: 'Yes. It held data of yours (the screen, the clipboard or a file outside the project).',
    no: 'No. It held none of your data.',
    unknown: 'Aqua did not say.',
  },
  reach: {
    yes: 'Yes. It used a tool that changes things or reaches out.',
    no: 'No. It used no tool that changes or sends anything.',
    unknown: 'Aqua did not say.',
  },
};

/** One mark of the strip in plain words, with when the run came to hold it. */
export function markSentence(kind: MarkKind, mark: Mark, when = ''): string {
  const base = MARK_SENTENCES[kind][mark];
  return mark === 'yes' && when !== '' ? `${base} It came to hold this ${when}.` : base;
}

export const ALL_THREE_SENTENCE = 'All three were there together. That is the combination an attack needs, so look at what this run did.';

// ---- the proof ----------------------------------------------------------------------------------

export type ProofVerdict = 'verified' | 'not_verified' | 'unchecked';

export interface RunProof {
  verdict: ProofVerdict;
  /** Lines of this run in the signed record. */
  lines: number;
  /** Of them, the ones under a signed tree head whose root matches. */
  covered: number;
  /** Lines that are in no signed head yet. */
  unsigned: number;
  /** Whether the whole record is one unbroken chain, or null when not said. */
  chainOk: boolean | null;
  heads: number;
  keyIds: string[];
  problems: string[];
}

const MAX_PROBLEMS = 10;

function problemList(...lists: unknown[]): string[] {
  const out: string[] = [];
  for (const list of lists) {
    if (!Array.isArray(list)) continue;
    for (const p of list) {
      if (typeof p !== 'string' || p.trim() === '') continue;
      const shown = cutText(shownText(p), 300);
      if (!out.includes(shown)) out.push(shown);
    }
  }
  return out.slice(0, MAX_PROBLEMS);
}

/** The proof from `{ "proof": {...}, "checks_out_here": bool, "problems": [...] }`, or null when the answer is not that. */
export function mapProof(body: unknown): RunProof | null {
  if (!isObject(body) || !isObject(body.proof)) return null;
  const p = body.proof;
  const lines = Array.isArray(p.lines) ? p.lines.length : 0;
  const problems = problemList(body.problems, p.problems);
  const checks = body.checks_out_here;
  const verdict: ProofVerdict = checks === true && problems.length === 0 && lines > 0 ? 'verified' : checks === false || problems.length > 0 ? 'not_verified' : 'unchecked';
  return {
    verdict,
    lines,
    covered: count(p.covered),
    unsigned: count(p.unsigned),
    chainOk: typeof p.chain_ok === 'boolean' ? p.chain_ok : null,
    heads: Array.isArray(p.heads) ? p.heads.length : 0,
    keyIds: Array.isArray(p.key_ids) ? p.key_ids.filter((k): k is string => typeof k === 'string').slice(0, 5).map((k) => shownText(k)) : [],
    problems,
  };
}

function linesText(n: number): string {
  return n === 1 ? '1 line' : `${n} lines`;
}

/** The verdict in words, never a tick alone: "Verified" or "Does not verify", then why. */
export function proofHeadline(p: RunProof): string {
  if (p.verdict === 'verified') return 'Verified. The signed record of this run matches what Aqua wrote.';
  if (p.verdict === 'not_verified') return 'Does not verify. The signed record of this run did not pass the check.';
  return 'Not checked. Aqua did not say whether the record checks out.';
}

/** What was found, one sentence each. */
export function proofDetails(p: RunProof): string[] {
  const out: string[] = [];
  out.push(`Under a signed tree head: ${p.covered} of ${linesText(p.lines)}.`);
  if (p.unsigned > 0) out.push(`Not signed yet: ${linesText(p.unsigned)}.`);
  if (p.chainOk === true) out.push('The chain of records is unbroken.');
  if (p.chainOk === false) out.push('The chain of records is broken.');
  return out;
}

export const PROOF_NOTE =
  'This is Aqua checking its own record with its own key. Somebody who has only your public key can check the same record: the command aqua-cli verify-run, followed by the run id, does it from a terminal.';

// ---- undo ---------------------------------------------------------------------------------------

export interface UndoReport {
  /** The undo itself worked (false: it failed, and the run's files were not all put back). */
  ok: boolean;
  dryRun: boolean;
  /** What was put back, or with a dry run what would be put back, one line each ("would restore C:/a.txt"). */
  restored: string[];
  /** Files that were left as they are, and why. */
  skipped: { path: string; why: string }[];
  /** The tools this run used whose effects no undo reaches (a command, a commit, a network call). */
  notUndoable: string[];
  /** The core's sentence when the undo failed, or what it said. */
  detail: string;
}

const MAX_LINES = 50;

/** The undo answer (`{ run_id, undo: <the tool result>, not_undoable, note }`), or null when it is not that. */
export function mapUndo(body: unknown, dryRun: boolean): UndoReport | null {
  if (!isObject(body) || !isObject(body.undo)) return null;
  const undo = body.undo;
  const data = isObject(undo.data) ? undo.data : {};
  const restored = Array.isArray(data.restored) ? data.restored.filter((r): r is string => typeof r === 'string').slice(0, MAX_LINES).map((r) => shownText(r)) : [];
  const skipped: { path: string; why: string }[] = [];
  if (Array.isArray(data.skipped)) {
    for (const s of data.skipped.slice(0, MAX_LINES)) {
      if (isObject(s) && typeof s.path === 'string') skipped.push({ path: shownText(s.path), why: shownText(s.why) });
    }
  }
  const notUndoable = Array.isArray(body.not_undoable) ? body.not_undoable.filter((t): t is string => typeof t === 'string').slice(0, 20).map((t) => shownText(t)) : [];
  const ok = undo.ok === true;
  const error = isObject(undo.error) ? asText(undo.error.message) : '';
  const detail = ok ? '' : cutText(shownText(error !== '' ? error : asText(undo.text)), 300);
  return { ok, dryRun: data.dry_run === true || dryRun, restored, skipped, notUndoable, detail };
}

/** The headline of an undo report. */
export function undoHeadline(r: UndoReport): string {
  if (!r.ok) return r.detail !== '' ? `The undo did not work: ${r.detail}` : 'The undo did not work.';
  const n = r.restored.length;
  if (r.dryRun) {
    if (n === 0) return 'Nothing from this run can be put back.';
    return n === 1 ? 'Aqua can put back 1 change. Nothing has been changed yet.' : `Aqua can put back ${n} changes. Nothing has been changed yet.`;
  }
  if (n === 0) return 'Nothing was put back.';
  return n === 1 ? 'Put back 1 change.' : `Put back ${n} changes.`;
}

/** The first dry run found something to put back, so the real one can be offered. */
export function canPutBack(r: UndoReport | null): boolean {
  return r !== null && r.ok && r.dryRun && r.restored.length > 0;
}

export const UNDO_RUNNING = 'This run is still going or waiting for you. It can be undone once it has ended.';

// ---- resume -------------------------------------------------------------------------------------

export type ResumeChoice = 'skip' | 'retry' | 'stop';

export const RESUME_CHOICES: ResumeChoice[] = ['skip', 'retry', 'stop'];

export const RESUME_LABELS: Record<ResumeChoice, string> = {
  skip: 'Skip it and go on',
  retry: 'Run it again',
  stop: 'Stop this run for good',
};

const RESUME_MEANING: Record<ResumeChoice, string> = {
  skip: 'Go on without running it. The agent is told it may or may not have happened, and the same call asks every time it is repeated.',
  retry: 'The agent may run it again, under the usual rules.',
  stop: 'End this run and do not offer it again.',
};

export interface PendingCall {
  step: number;
  tool: string;
  arguments: string;
}

export type ResumeOutcome =
  | { kind: 'continued'; continuedBy: string | null; answer: string; question: { text: string; choices: string[] } | null; latencyMs: number | null }
  | { kind: 'stopped'; note: string }
  | { kind: 'needs_choice'; sentence: string; calls: PendingCall[]; choices: ResumeChoice[]; meaning: Record<ResumeChoice, string> };

const MAX_ANSWER_CHARS = 2000;

/**
 * What the resume route answered. A 200 is a run that went on (or one that was ended on purpose); a 409 with a question is calls that were running when
 * the run stopped and may have changed something, which the person decides about. Anything else is null: the caller makes a sentence of it.
 */
export function mapResume(status: number, body: unknown): ResumeOutcome | null {
  if (!isObject(body)) return null;
  if (status >= 200 && status < 300) {
    if (body.state === 'cancelled') return { kind: 'stopped', note: shownText(body.note) };
    // A run that went on always says which run it was and what it answered; an answer that says neither is not one to report as done.
    if (typeof body.run_id !== 'string' || typeof body.answer !== 'string') return null;
    const q = body.question;
    const question =
      isObject(q) && typeof q.text === 'string'
        ? { text: shownText(q.text), choices: Array.isArray(q.choices) ? q.choices.filter((c): c is string => typeof c === 'string').slice(0, 8).map((c) => shownText(c)) : [] }
        : null;
    const by = asText(body.continued_by).trim();
    return {
      kind: 'continued',
      continuedBy: by !== '' ? by : null,
      answer: cutText(shownText(body.answer, true), MAX_ANSWER_CHARS),
      question,
      latencyMs: typeof body.latency_ms === 'number' && Number.isFinite(body.latency_ms) && body.latency_ms >= 0 ? Math.round(body.latency_ms) : null,
    };
  }
  if (status === 409 && isObject(body.question) && Array.isArray(body.question.calls)) {
    const q = body.question;
    const offered = Array.isArray(q.choices) ? RESUME_CHOICES.filter((c) => (q.choices as unknown[]).includes(c)) : [];
    if (offered.length === 0) return null;
    const meaning = { ...RESUME_MEANING };
    if (isObject(q.meaning)) {
      for (const c of RESUME_CHOICES) {
        const m = q.meaning[c];
        if (typeof m === 'string' && m.trim() !== '') meaning[c] = cutText(shownText(m), 300);
      }
    }
    const calls: PendingCall[] = [];
    for (const c of (q.calls as unknown[]).slice(0, 20)) {
      if (!isObject(c)) continue;
      calls.push({ step: count(c.step), tool: shownText(c.tool), arguments: argumentsText(c.arguments) });
    }
    return { kind: 'needs_choice', sentence: serverSentence(body) || 'Some calls were running when this run stopped, and they may have changed something.', calls, choices: offered, meaning };
  }
  return null;
}

/** The sentence for a run that went on. */
export function continuedSentence(o: Extract<ResumeOutcome, { kind: 'continued' }>): string {
  return o.continuedBy !== null ? `Went on as the new run ${o.continuedBy}.` : 'Went on with this run.';
}

// ---- the calls ----------------------------------------------------------------------------------

const RUN_STATES = new Set(['running', 'completed', 'failed', 'cancelled', 'interrupted', 'waiting_user', 'expired']);

export interface ListOptions {
  limit?: number;
  /** One of the states of a run; anything else is left out of the request. */
  status?: string;
  /** Only the runs that stopped and can be gone on from. */
  resumable?: boolean;
}

export const DEFAULT_LIMIT = 30;
export const MAX_LIMIT = 500;

export function listUrl(options: ListOptions = {}): string {
  const p: string[] = [];
  const limit = Number.isSafeInteger(options.limit) && (options.limit as number) > 0 ? Math.min(options.limit as number, MAX_LIMIT) : DEFAULT_LIMIT;
  p.push(`limit=${limit}`);
  if (options.status && RUN_STATES.has(options.status)) p.push(`status=${options.status}`);
  if (options.resumable) p.push('resumable=1');
  return `/api/v1/runs?${p.join('&')}`;
}

/** The path of one run. A run id is the core's own, but it is encoded all the same: an id with a slash or dots must not change which route is called. */
export function runPath(id: string, tail = ''): string {
  return `/api/v1/runs/${encodeURIComponent(id)}${tail}`;
}

function noId(): PlainError {
  return new PlainError('That run cannot be opened.');
}

const LIST_WORDING = { fallback: 'The runs could not be read.', notFound: 'This version of Aqua cannot list runs.' };
const RUN_WORDING = { fallback: 'That run could not be read.', notFound: 'That run is not there any more.' };

export const runsApi = {
  /** The runs, newest first (UI token). Throws a PlainError whose message is a sentence. */
  list: async (options: ListOptions = {}): Promise<RunSummary[]> => {
    const answer = await sendRequest(listUrl(options));
    if (!answer.ok) throw failure(answer, LIST_WORDING);
    const runs = mapRunList(answer.body);
    if (!runs) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return runs;
  },

  /** The ids of the runs that stopped and can be gone on from. */
  resumableIds: async (): Promise<Set<string>> => {
    const runs = await runsApi.list({ resumable: true, limit: MAX_LIMIT });
    return new Set(runs.map((r) => r.id));
  },

  get: async (id: string): Promise<RunDetail> => {
    if (id === '') throw noId();
    const answer = await sendRequest(runPath(id));
    if (!answer.ok) throw failure(answer, RUN_WORDING);
    const run = mapRunDetail(answer.body);
    if (!run) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return run;
  },

  safety: async (id: string): Promise<RunSafety> => {
    if (id === '') throw noId();
    const answer = await sendRequest(runPath(id, '/safety'));
    if (!answer.ok) throw failure(answer, { fallback: 'What this run could do could not be read.', notFound: RUN_WORDING.notFound });
    const safety = mapSafety(answer.body);
    if (!safety) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return safety;
  },

  /** The signed record of the run and whether it checks out. Reading it seals the ledger, so it is asked for on a click, not on its own. */
  proof: async (id: string): Promise<RunProof> => {
    if (id === '') throw noId();
    const answer = await sendRequest(runPath(id, '/proof'));
    if (!answer.ok) throw failure(answer, { fallback: 'The signed record could not be checked.', notFound: 'The signed record holds no line of this run.' });
    const proof = mapProof(answer.body);
    if (!proof) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return proof;
  },

  /** Says what an undo would put back (`dryRun`), or puts it back. Both go through the tool host as the app, so Aqua may ask first. */
  undo: async (id: string, dryRun: boolean): Promise<UndoReport> => {
    if (id === '') throw noId();
    const answer = await sendRequest(runPath(id, '/undo'), jsonInit('POST', { dry_run: dryRun }));
    // 422 is an undo that was tried and failed: its body has the same shape.
    if (answer.ok || answer.status === 422) {
      const report = mapUndo(answer.body, dryRun);
      if (report) return report;
      if (answer.ok) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    }
    throw failure(answer, { fallback: 'The undo could not be done.', notFound: RUN_WORDING.notFound });
  },

  /**
   * Goes on with a run that stopped. With no `choice` the first call is asked; when calls were running and may have changed something the answer is a
   * question (`needs_choice`) and the person's choice comes back here. The request lasts as long as the continuation does.
   */
  resume: async (id: string, choice?: ResumeChoice): Promise<ResumeOutcome> => {
    if (id === '') throw noId();
    if (choice !== undefined && !RESUME_CHOICES.includes(choice)) throw new PlainError('That is not one of the choices.');
    // The body is `{}` the first time (the contract's `unknown` is optional), so that the request is a JSON one whichever way the core reads a missing body.
    const answer: HttpAnswer = await sendRequest(runPath(id, '/resume'), jsonInit('POST', choice === undefined ? {} : { unknown: choice }));
    const outcome = mapResume(answer.status, answer.body);
    if (outcome) return outcome;
    if (answer.ok) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    throw new PlainError(failureSentence(answer.status, serverSentence(answer.body), { fallback: 'The run could not be gone on from.', notFound: RUN_WORDING.notFound }), answer.status);
  },
};
