// How the consents, permissions, queue and spoken-announcement screens talk to the core. Their routes need the UI token; the desktop
// shell (or the dev proxy) adds it to every /api request, so nothing here builds a header, reads a key or keeps one (see api/gateway.ts).
//
// A call never throws by itself: `callCore` answers with the body, or with one plain sentence for the person (never a status line, a
// stack or the raw body). The api modules turn that sentence into an `AquaError`, the only kind of error a screen shows.
// Relative imports in `src/api` carry the `.ts` extension so that `node --test` can read them without a build.

/** An error whose message is already a sentence for the person reading it: a screen shows `message` and nothing else. */
export class AquaError extends Error {
  /** The HTTP status, or null when no answer came at all. */
  status: number | null;
  constructor(message: string, status: number | null = null) {
    super(message);
    this.name = 'AquaError';
    this.status = status;
  }
}

export const UNREADABLE_ANSWER = 'Aqua sent an answer the app could not read.';
const NOT_ASKED = 'That did not work.';

/** What to show for anything that was thrown: a sentence we wrote, or a plain one for a failure nobody planned for. */
export function errorSentence(error: unknown): string {
  return error instanceof AquaError ? error.message : NOT_ASKED;
}

export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** A string, or '' when the core sent anything else. */
export function text(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

/** A count: a whole number from zero, or zero when the core did not say. */
export function count(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

/** A number the core can be asked about (a row id that goes into a path or a body). */
export function isWholeNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

/** A time or a length that is positive, or null when the core sent none (zero means "never"). */
export function positiveOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : null;
}

/** The strings of a list, in order; anything else in it is left out; not a list is empty. */
export function stringList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((x): x is string => typeof x === 'string') : [];
}

/** At most `max` characters, cut with an ellipsis (whole characters, never half of one). */
export function clip(value: string, max: number): string {
  const letters = Array.from(value);
  return letters.length > max ? `${letters.slice(0, max - 1).join('')}…` : value;
}

/** The core's own sentence from an error body (`{"error": "..."}`), or '' when there is none or it is longer than `max`. */
export function serverSentence(body: unknown, max = 200): string {
  if (!isObject(body) || typeof body.error !== 'string') return '';
  const sentence = body.error.trim();
  return sentence !== '' && sentence.length <= max ? sentence : '';
}

function tidy(sentence: string): string {
  const first = sentence.charAt(0).toUpperCase() + sentence.slice(1);
  return /[.!?]$/.test(first) ? first : `${first}.`;
}

/** The sentences of one call: what to say when nothing else fits, and what to say for statuses this call has a meaning for. */
export interface Wording {
  fallback: string;
  status?: Partial<Record<number, string>>;
  /** How long the core's own sentence may be to be shown (default 200 characters). */
  serverMax?: number;
}

/**
 * The sentence for a failed call. `status` is the HTTP status, or null when no answer came at all. A person is shown neither a status
 * line nor a stack: no answer, a refused key and a missing UI token have sentences of their own (the core's wording about tokens is
 * not repeated); then the call's own sentence for that status; then the core's sentence when it sent a short one; then a plain
 * sentence for the status.
 */
export function failureSentence(status: number | null, serverMessage: string, wording: Wording): string {
  if (status === null) return 'Could not reach Aqua. Check that it is running, then try again.';
  if (status === 401) return "Aqua did not accept the app's key. Restart Aqua and try again.";
  if (status === 403) return 'Only the Aqua app can do this. Open it from the Aqua window, not from a script or another program.';
  const own = wording.status?.[status];
  if (own) return own;
  if (serverMessage !== '') return tidy(serverMessage);
  if (status === 404) return 'This version of Aqua cannot do that.';
  if (status === 429) return 'Aqua is being asked too often. Wait a moment and try again.';
  if (status === 503) return 'Aqua is busy or still starting. Try again in a moment.';
  if (status === 504) return 'Aqua took too long to answer. Try again.';
  return wording.fallback;
}

export type Answer =
  | { ok: true; status: number; body: unknown }
  | { ok: false; status: number | null; body: unknown; sentence: string };

/** One call to the core. The body is read as JSON when it is JSON (a 204, an empty body or a page is `null`). It never throws. */
export async function callCore(url: string, init: RequestInit | undefined, wording: Wording): Promise<Answer> {
  let response: Response;
  try {
    response = await fetch(url, init);
  } catch {
    return { ok: false, status: null, body: null, sentence: failureSentence(null, '', wording) };
  }
  let body: unknown = null;
  try {
    body = await response.json();
  } catch {
    /* not JSON: a 204, an empty body or an error page */
  }
  if (!response.ok) {
    return { ok: false, status: response.status, body, sentence: failureSentence(response.status, serverSentence(body, wording.serverMax), wording) };
  }
  return { ok: true, status: response.status, body };
}

/** The body of an answer that went well, or the error a screen shows. */
export function bodyOrThrow(answer: Answer): unknown {
  if (!answer.ok) throw new AquaError(answer.sentence, answer.status);
  return answer.body;
}

/** The request init for a JSON body (a body of `undefined` sends none, and no content type). */
export function jsonInit(method: 'POST' | 'PUT' | 'DELETE', body?: unknown): RequestInit {
  return body === undefined ? { method } : { method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) };
}

const localDate = (unix: number) => new Date(unix * 1000).toLocaleDateString();

/** "just now", "5 min ago", "3 h ago", and from two days on the date. `nowSecs` is given so the answer does not depend on the clock. */
export function agoText(unixSecs: number, nowSecs: number, formatDate: (unix: number) => string = localDate): string {
  const s = Math.max(0, nowSecs - unixSecs);
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 86_400 * 2) return `${Math.round(s / 3600)} h ago`;
  return formatDate(unixSecs);
}

/** A length of time for a person: "40 s", "3 min", "1 h 5 min". */
export function spanText(totalSecs: number): string {
  const s = Math.max(0, Math.round(totalSecs));
  if (s < 60) return `${s} s`;
  const minutes = Math.floor(s / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${hours} h` : `${hours} h ${rest} min`;
}
