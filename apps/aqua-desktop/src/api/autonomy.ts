// How much Aqua asks, and how much one run may do: GET and PUT /api/v1/autonomy (documentation/AUTONOMY.md, card R7-18). The GET may be read
// with either token; the PUT needs the UI token, which the shell or the dev proxy adds, so nothing here builds a header or keeps a key.
//
// A level can only make Aqua ask MORE than it does by default, and a cap can only stop a run sooner: nothing offered here lets Aqua do what
// the rules of today do not allow. The three levels are listed in the order of how much they ask, today's rules first.
// Everything here is a plain function, so a test can call it without a page. Text from the core (a run id, a level's words, a note) is only
// ever handed to React as a text child.

import { AquaError, agoText, bodyOrThrow, callCore, count, isObject, jsonInit, positiveOrNull, spanText, stringList, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

export type LevelId = 'ask_risky' | 'ask_outside' | 'ask_all';

/** From the level that asks least (today's rules) to the one that asks most; each asks at least what the one before it does. */
export const LEVEL_ORDER: readonly LevelId[] = ['ask_risky', 'ask_outside', 'ask_all'];
export const DEFAULT_LEVEL: LevelId = 'ask_risky';

export const LEVEL_NAMES: Record<LevelId, string> = {
  ask_all: 'Ask me about everything',
  ask_risky: "Ask me about risky things (today's behaviour)",
  ask_outside: 'Ask me about anything that reaches outside',
};

export type CapId = 'max_seconds' | 'max_tool_calls' | 'max_files_changed' | 'max_changed_bytes';
export const CAP_IDS: readonly CapId[] = ['max_seconds', 'max_tool_calls', 'max_files_changed', 'max_changed_bytes'];
export type CapNumbers = Record<CapId, number>;

/** What each cap is called on the screen, and what it counts (documentation/AUTONOMY.md, "What counts, and where"). */
export const CAP_INFO: Record<CapId, { label: string; unit: string; counts: string; stop: string }> = {
  max_seconds: { label: 'Time', unit: 'seconds', counts: 'Seconds since the run made its first call.', stop: 'time' },
  max_tool_calls: { label: 'Tool calls', unit: 'calls', counts: 'Tool calls the run asked for, of any tool.', stop: 'tool calls' },
  max_files_changed: { label: 'Files changed', unit: 'files', counts: 'Different files the run changed. A file counts once.', stop: 'files changed' },
  max_changed_bytes: { label: 'Bytes changed', unit: 'bytes', counts: 'Bytes the run wrote or changed.', stop: 'bytes changed' },
};

// The numbers of documentation/AUTONOMY.md. They are used only for a number the core's answer leaves out.
const DOCUMENTED_DEFAULTS: CapNumbers = { max_seconds: 7_200, max_tool_calls: 200, max_files_changed: 500, max_changed_bytes: 1_073_741_824 };
const DOCUMENTED_MOST: CapNumbers = { max_seconds: 86_400, max_tool_calls: 10_000, max_files_changed: 100_000, max_changed_bytes: 107_374_182_400 };

export interface LevelInfo {
  id: LevelId;
  /** The words of the screen. */
  name: string;
  /** The core's own text for the level; '' when the core did not send it. */
  what: string;
}

export interface RunningRun {
  runId: string;
  toolCalls: number;
  seconds: number;
  filesChanged: number;
  bytesChanged: number;
  /** The first cap the run reached, or null. */
  stoppedBy: CapId | null;
}

export interface StopRecord {
  runId: string;
  /** The cap that stopped the run, or null for a cap this app does not know. */
  cap: CapId | null;
  limit: number;
  atMs: number | null;
}

export interface AutonomyView {
  level: LevelId;
  /** The three levels, in the order of `LEVEL_ORDER`. */
  levels: LevelInfo[];
  caps: CapNumbers;
  defaults: CapNumbers;
  /** The most each cap may be. */
  most: CapNumbers;
  /** The least any cap may be. */
  least: number;
  /** Whether the setting has been saved (otherwise the defaults are in use). */
  saved: boolean;
  /** The level the tool host's policy is applying, or null when the core did not say. */
  appliedLevel: LevelId | null;
  running: RunningRun[];
  /** The last runs a cap stopped, newest first. */
  recentStops: StopRecord[];
  notBuilt: string[];
}

export function levelId(value: unknown): LevelId | null {
  return LEVEL_ORDER.find((l) => l === value) ?? null;
}

export function capId(value: unknown): CapId | null {
  return CAP_IDS.find((c) => c === value) ?? null;
}

/** A whole number from 1, or null. */
function wholeFromOne(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 1 ? value : null;
}

/** The four numbers of an object; one that is missing or not a whole number from 1 is the fallback's. */
function capNumbers(raw: unknown, fallback: CapNumbers): CapNumbers {
  const o = isObject(raw) ? raw : {};
  const out = { ...fallback };
  for (const cap of CAP_IDS) out[cap] = wholeFromOne(o[cap]) ?? fallback[cap];
  return out;
}

/**
 * The answer of GET and PUT as the screen uses it, or null when it is not one: there is no level the app knows, or no caps. A number the
 * core leaves out is the documented one; a level the app does not know is not listed (the core would refuse it).
 */
export function mapAutonomy(body: unknown): AutonomyView | null {
  if (!isObject(body)) return null;
  const level = levelId(body.level);
  if (level === null || !isObject(body.caps)) return null;
  const defaults = capNumbers(body.defaults, DOCUMENTED_DEFAULTS);
  const bounds = isObject(body.bounds) ? body.bounds : {};
  const rows: unknown[] = Array.isArray(body.levels) ? body.levels : [];
  const levels = LEVEL_ORDER.map((id): LevelInfo => {
    const row = rows.find((r) => isObject(r) && r.id === id);
    return { id, name: LEVEL_NAMES[id], what: isObject(row) ? text(row.what) : '' };
  });
  const running = (Array.isArray(body.running) ? body.running : []).filter(isObject).map((r): RunningRun => ({
    runId: text(r.run_id),
    toolCalls: count(r.tool_calls),
    seconds: count(r.seconds),
    filesChanged: count(r.files_changed),
    bytesChanged: count(r.bytes_changed),
    stoppedBy: capId(r.stopped_by),
  }));
  const recentStops = (Array.isArray(body.recent_stops) ? body.recent_stops : [])
    .filter(isObject)
    .map((s): StopRecord => ({ runId: text(s.run_id), cap: capId(s.cap), limit: count(s.limit), atMs: positiveOrNull(s.at_unix_ms) }))
    .sort((a, b) => (b.atMs ?? 0) - (a.atMs ?? 0));
  return {
    level,
    levels,
    caps: capNumbers(body.caps, defaults),
    defaults,
    most: capNumbers(bounds.most, DOCUMENTED_MOST),
    least: wholeFromOne(bounds.least) ?? 1,
    saved: body.saved === true,
    appliedLevel: levelId(body.applied_level),
    running,
    recentStops,
    notBuilt: stringList(body.not_built),
  };
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** The core's text for a level cut at its first sentence; what follows it is the rest. Whitespace is made single. */
export function splitSentence(value: string): { first: string; rest: string } {
  const flat = value.trim().replace(/\s+/g, ' ');
  const found = /^(.*?[.!?])(?=\s|$)\s*(.*)$/.exec(flat);
  return found ? { first: found[1], rest: found[2] } : { first: flat, rest: '' };
}

/** 1234567 as "1,234,567" (not the computer's locale, so that a test gives one answer). */
export function numberText(n: number): string {
  return String(Math.trunc(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** A size for a person: "512 bytes", "1.5 KiB", "1 GiB". */
export function bytesText(n: number): string {
  const units = ['bytes', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = Math.max(0, n);
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  if (unit === 0) return value === 1 ? '1 byte' : `${numberText(value)} bytes`;
  return `${Math.round(value * 10) / 10} ${units[unit]}`;
}

/** A cap's number with what it means in other words: seconds as a length of time, bytes as a size. */
export function capValueText(cap: CapId, n: number): string {
  if (cap === 'max_seconds') return `${numberText(n)} (${spanText(n)})`;
  if (cap === 'max_changed_bytes') return `${numberText(n)} (${bytesText(n)})`;
  return numberText(n);
}

/** A cap's number as short as it can be said: seconds as a length of time, bytes as a size. */
export function capShortText(cap: CapId, n: number): string {
  if (cap === 'max_seconds') return spanText(n);
  if (cap === 'max_changed_bytes') return bytesText(n);
  return numberText(n);
}

/** A sentence when the level that was saved is not the one the part of Aqua that asks questions is applying; '' when they agree or the core does not say. */
export function levelMismatch(view: AutonomyView): string {
  if (view.appliedLevel === null || view.appliedLevel === view.level) return '';
  return `The saved level is "${LEVEL_NAMES[view.level]}", but the part of Aqua that decides what to ask about is applying "${LEVEL_NAMES[view.appliedLevel]}". The saved level is not in force yet.`;
}

const plural = (n: number, one: string, many: string) => `${numberText(n)} ${n === 1 ? one : many}`;

/** One line about a run that is going: what it has used so far. */
export function runLine(run: RunningRun): string {
  const used = [
    plural(run.toolCalls, 'tool call', 'tool calls'),
    spanText(run.seconds),
    `${plural(run.filesChanged, 'file', 'files')} changed`,
    `${bytesText(run.bytesChanged)} changed`,
  ].join(', ');
  const stopped = run.stoppedBy === null ? '' : `. It reached its limit of ${CAP_INFO[run.stoppedBy].stop} and was stopped`;
  return `${used}${stopped}.`;
}

/** One line about a run a cap stopped. `nowMs` is given so the answer does not depend on the clock. */
export function stopLine(stop: StopRecord, nowMs: number, formatDate?: (unix: number) => string): string {
  const what = stop.cap === null ? 'a limit this version of the app does not know' : `its limit of ${CAP_INFO[stop.cap].stop} (${capShortText(stop.cap, stop.limit)})`;
  const when = stop.atMs === null ? '' : `, ${agoText(stop.atMs / 1000, nowMs / 1000, formatDate)}`;
  return `Stopped at ${what}${when}.`;
}

/** The sentence after a save that went well. */
export function savedText(levelChanged: boolean): string {
  return levelChanged
    ? 'Saved. Aqua uses this now. Your earlier "approve for this session" answers are forgotten, so that an answer given under the old level does not cover a question the new one asks.'
    : 'Saved. Aqua uses this now.';
}

// ---- the form --------------------------------------------------------------------------------------------------------------------------

/** What the person has typed and chosen so far; a cap is the text of its box. */
export interface Draft {
  level: LevelId;
  caps: Record<CapId, string>;
}

export function draftFrom(view: AutonomyView): Draft {
  return {
    level: view.level,
    caps: { max_seconds: String(view.caps.max_seconds), max_tool_calls: String(view.caps.max_tool_calls), max_files_changed: String(view.caps.max_files_changed), max_changed_bytes: String(view.caps.max_changed_bytes) },
  };
}

/** The caps put back to the defaults, in the draft (nothing is saved until the person saves). */
export function withDefaultCaps(draft: Draft, view: AutonomyView): Draft {
  return { ...draft, caps: draftFrom({ ...view, caps: view.defaults }).caps };
}

/** What a box says as a number, or null when it is not a whole number of plain digits (no sign, no point, no exponent) that a computer can hold exactly. */
export function parseCap(value: string): number | null {
  const t = value.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  return Number.isSafeInteger(n) ? n : null;
}

/** Why a box cannot be saved, or '' when it can. */
export function capProblem(cap: CapId, value: string, view: AutonomyView): string {
  const n = parseCap(value);
  if (n !== null && n >= view.least && n <= view.most[cap]) return '';
  return `${CAP_INFO[cap].label}: enter a whole number from ${numberText(view.least)} to ${numberText(view.most[cap])}.`;
}

/** The boxes that cannot be saved, each with its sentence. */
export function draftProblems(view: AutonomyView, draft: Draft): Partial<Record<CapId, string>> {
  const out: Partial<Record<CapId, string>> = {};
  for (const cap of CAP_IDS) {
    const problem = capProblem(cap, draft.caps[cap], view);
    if (problem !== '') out[cap] = problem;
  }
  return out;
}

/** Whether the draft differs from what is saved. A box that is not a number counts as different. */
export function isDirty(view: AutonomyView, draft: Draft): boolean {
  return draft.level !== view.level || CAP_IDS.some((cap) => parseCap(draft.caps[cap]) !== view.caps[cap]);
}

export type SavePlan = { ok: true; body: Record<string, unknown>; levelChanged: boolean } | { ok: false; problems: string[] };

/**
 * The body of the PUT: only what changed. A cap that is now the default is sent as null ("back to the default"), so that it follows the
 * default if that ever moves; any other number is sent as it is.
 */
export function planSave(view: AutonomyView, draft: Draft): SavePlan {
  // Only the three levels exist, and each asks at least as much as today's rules: a level that is none of them is never sent.
  const levelProblem = levelId(draft.level) === null ? ['Choose one of the three levels.'] : [];
  const problems = [...levelProblem, ...Object.values(draftProblems(view, draft)).filter((p): p is string => typeof p === 'string')];
  if (problems.length > 0) return { ok: false, problems };
  const body: Record<string, unknown> = {};
  if (draft.level !== view.level) body.level = draft.level;
  for (const cap of CAP_IDS) {
    const n = parseCap(draft.caps[cap]) as number;
    if (n !== view.caps[cap]) body[cap] = n === view.defaults[cap] ? null : n;
  }
  return { ok: true, body, levelChanged: 'level' in body };
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

const NO_SETTING = 'This version of Aqua has no autonomy setting.';
const GET_WORDING: Wording = { fallback: 'The autonomy setting could not be read.', status: { 404: NO_SETTING } };
const PUT_WORDING: Wording = { fallback: 'The autonomy setting could not be saved.', status: { 404: NO_SETTING, 500: 'The autonomy setting could not be saved.' } };

export const autonomyApi = {
  /** The level, the caps and the runs that are going (either token). */
  get: async (): Promise<AutonomyView> => {
    const view = mapAutonomy(bodyOrThrow(await callCore('/api/v1/autonomy', undefined, GET_WORDING)));
    if (!view) throw new AquaError(UNREADABLE_ANSWER);
    return view;
  },
  /**
   * Saves what `body` names (UI token): see `planSave`. The answer is the same as the GET's; when it was saved and the answer cannot be read,
   * the result is null and the screen reads the setting again.
   */
  save: async (body: Record<string, unknown>): Promise<AutonomyView | null> => {
    if (Object.keys(body).length === 0) throw new AquaError('Nothing was changed, so there is nothing to save.');
    return mapAutonomy(bodyOrThrow(await callCore('/api/v1/autonomy', jsonInit('PUT', body), PUT_WORDING)));
  },
};
