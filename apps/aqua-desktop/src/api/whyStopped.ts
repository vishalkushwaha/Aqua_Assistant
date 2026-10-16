// Why a run stopped, as the run view shows it (documentation/TRIAGE.md; `GET /api/v1/runs/{id}` carries `why_it_stopped`, `next_steps`, `triage`, `diagnosis`
// and `stopped_by`). Everything here is a plain function except the one read of the autonomy setting, so a test can call it without a page.
//
// The sentence and the steps are the core's own words (rules, no model). The model's guess, when there is one, is a suggestion that nothing has run: it is
// labelled so, and never takes the place of the sentence. A run that went well, is still going or waits for an answer has none of these, and neither has a run
// recorded before they existed: that is `null`, and the view shows nothing more than it did.
//
// A limit of the autonomy setting that stopped a run is named in words, with its number when the core still knows it:
//   GET /api/v1/autonomy   `recent_stops` lists the last runs a limit stopped, with the limit in force at that moment; `caps` is the limit set now.
// The number of a stop is exact. The limit set now is only said as such: it may have been changed since the run ended.

import { callCore, isObject, type Wording } from './coreRequest.ts';
import { cutText, shownText } from './plainErrors.ts';

const MAX_SENTENCE_CHARS = 400;
const MAX_STEP_CHARS = 160;
const MAX_STEPS = 3;
const MAX_GUESS_CHARS = 700;

/** The label over the model's guess: it is a suggestion, and nothing in it has been done. */
export const GUESS_LABEL = "The model's guess (not run)";

export interface WhyStopped {
  /** The core's one sentence ('' when the run has none). */
  sentence: string;
  /** Up to three things the person can do, in words. */
  nextSteps: string[];
  /** The limit of the autonomy setting that stopped the run (`max_seconds`, `max_tool_calls`, `max_files_changed`, `max_changed_bytes`), or null. */
  cap: string | null;
  /** Whether a plain retry may help, or null when the core does not say. */
  retryable: boolean | null;
  /** The language model's guess, when `AQUA_DIAGNOSE` was on and the rules had nothing to say. Never run. */
  guess: string | null;
}

function oneLine(value: unknown, max: number): string {
  return typeof value === 'string' ? cutText(shownText(value).trim(), max) : '';
}

/**
 * What the run record says about why the run stopped, or null when it says nothing (a run that went well, a run from before this existed, or something that
 * is not a run). A step or a sentence of the wrong type is left out; a missing field is never made up.
 */
export function mapWhyStopped(body: unknown): WhyStopped | null {
  if (!isObject(body)) return null;
  const triage = isObject(body.triage) ? body.triage : {};
  const sentence = oneLine(body.why_it_stopped, MAX_SENTENCE_CHARS) || oneLine(triage.why, MAX_SENTENCE_CHARS);
  const rawSteps = Array.isArray(body.next_steps) ? body.next_steps : Array.isArray(triage.next_steps) ? triage.next_steps : [];
  const nextSteps: string[] = [];
  for (const step of rawSteps) {
    const text = oneLine(step, MAX_STEP_CHARS);
    if (text !== '' && nextSteps.length < MAX_STEPS) nextSteps.push(text);
  }
  const capText = oneLine(body.stopped_by, 40);
  const d = isObject(body.diagnosis) ? body.diagnosis : null;
  // `rules_stand` is the model being asked and having nothing usable: no guess to show.
  const guessText = d !== null && d.outcome !== 'rules_stand' ? oneLine(d.suggestion, MAX_GUESS_CHARS) : '';
  if (sentence === '' && nextSteps.length === 0 && capText === '' && guessText === '') return null;
  return {
    sentence,
    nextSteps,
    cap: capText !== '' ? capText : null,
    retryable: typeof triage.retryable === 'boolean' ? triage.retryable : null,
    guess: guessText !== '' ? guessText : null,
  };
}

// ---- the limit that stopped a run -----------------------------------------------------------------

export interface LimitInfo {
  limit: number;
  /** True: this is the limit in force when the run was stopped. False: it is the limit set now, which may have been changed since. */
  exact: boolean;
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? `1 ${one}` : `${n} ${many}`;
}

/** "30 seconds", "5 minutes", "2 hours", "1 h 30 min". */
export function secondsText(seconds: number): string {
  const n = Math.floor(seconds);
  if (n < 60) return plural(n, 'second', 'seconds');
  if (n < 3600) {
    const rest = n % 60;
    return rest === 0 ? plural(n / 60, 'minute', 'minutes') : `${Math.floor(n / 60)} min ${rest} s`;
  }
  const minutes = Math.floor((n % 3600) / 60);
  return minutes === 0 ? plural(n / 3600, 'hour', 'hours') : `${Math.floor(n / 3600)} h ${minutes} min`;
}

/** "300 bytes", "1 GB", "1.5 MB" (powers of 1024). */
export function bytesText(bytes: number): string {
  const n = Math.floor(bytes);
  if (n < 1024) return plural(n, 'byte', 'bytes');
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${Number.isInteger(value) ? value : value.toFixed(1)} ${units[unit]}`;
}

const CAP_WORDS: Record<string, { what: string; value: (n: number) => string }> = {
  max_seconds: { what: 'how long a run may take', value: (n) => `${secondsText(n)} of running time` },
  max_tool_calls: { what: 'how many tool calls a run may make', value: (n) => plural(Math.floor(n), 'tool call', 'tool calls') },
  max_files_changed: { what: 'how many files a run may change', value: (n) => plural(Math.floor(n), 'changed file', 'changed files') },
  max_changed_bytes: { what: 'how much a run may write', value: (n) => `${bytesText(n)} written` },
};

/**
 * The limit that stopped a run, in a sentence: "This run reached its limit of 200 tool calls." When the number is not known it says which limit; when only the
 * limit set now is known it says that it is that one. A limit the page does not know is still said, without a name.
 */
export function capSentence(cap: string, info: LimitInfo | null): string {
  const words = CAP_WORDS[cap];
  if (!words) return 'This run reached a limit of the autonomy setting.';
  if (info === null || !Number.isFinite(info.limit) || info.limit <= 0) return `This run reached the limit on ${words.what}.`;
  if (info.exact) return `This run reached its limit of ${words.value(info.limit)}.`;
  return `This run reached the limit on ${words.what}. The limit set now is ${words.value(info.limit)}, which may have been changed since.`;
}

/** The limit from the autonomy setting's answer: the stop that was noted for this run if there is one, else the limit set now, else nothing. */
export function limitFromAutonomy(body: unknown, runId: string, cap: string): LimitInfo | null {
  if (!isObject(body)) return null;
  const positive = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v) && v > 0;
  if (Array.isArray(body.recent_stops)) {
    for (const stop of body.recent_stops) {
      if (isObject(stop) && stop.run_id === runId && stop.cap === cap && positive(stop.limit)) return { limit: stop.limit, exact: true };
    }
  }
  if (isObject(body.caps) && positive(body.caps[cap])) return { limit: body.caps[cap] as number, exact: false };
  return null;
}

const AUTONOMY: Wording = { fallback: 'The limits could not be read.' };

export const whyStoppedApi = {
  /** The number behind a limit that stopped a run. It never throws: a number that cannot be read is simply not shown. */
  limitFor: async (runId: string, cap: string): Promise<LimitInfo | null> => {
    const answer = await callCore('/api/v1/autonomy', undefined, AUTONOMY);
    return answer.ok ? limitFromAutonomy(answer.body, runId, cap) : null;
  },
};
