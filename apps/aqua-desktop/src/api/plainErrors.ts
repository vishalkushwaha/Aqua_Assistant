// What a failed call to the core is told to the person as: one plain sentence, never a status line, a stack or a raw body.
//
// Shared by the approval card, the run view and the steering box. Everything here is a plain function, so a test can call it without a
// page. A call is made with `sendRequest`, which turns "no answer at all" into a sentence; the caller decides what a not-ok answer means
// (the approval card, for one, has to tell "too late" from "answered already") or hands it to `failureSentence`.

export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** An error whose message is already a sentence for the person reading it. `status` is the HTTP status, or null when no answer came. */
export class PlainError extends Error {
  readonly status: number | null;
  constructor(message: string, status: number | null = null) {
    super(message);
    this.name = 'PlainError';
    this.status = status;
  }
}

export const NO_ANSWER = 'Could not reach Aqua. Check that it is running, then try again.';
export const UNREADABLE_ANSWER = 'Aqua sent an answer the app could not read.';
export const NOT_DONE = 'That did not work.';

/** The core's own sentence from an error body (`{"error": "..."}`), or '' when there is none or it is not a short sentence. */
export function serverSentence(body: unknown): string {
  if (!isObject(body) || typeof body.error !== 'string') return '';
  const text = body.error.trim();
  return text !== '' && text.length <= 200 ? text : '';
}

/** A capital at the start and a full stop at the end. */
export function tidy(sentence: string): string {
  const first = sentence.charAt(0).toUpperCase() + sentence.slice(1);
  return /[.!?]$/.test(first) ? first : `${first}.`;
}

export interface Wording {
  /** For an answer that has no sentence of its own. */
  fallback: string;
  /** For a 404 that comes with no sentence (an older core that has not got the route). */
  notFound?: string;
}

/**
 * The sentence for a not-ok answer. A refusal about keys is always told in our own words (the core's wording about tokens is not repeated);
 * for the other statuses the core's own sentence is used when it sent a short one.
 */
export function failureSentence(status: number | null, serverMessage: string, wording: Wording): string {
  if (status === null) return NO_ANSWER;
  if (status === 401) return "Aqua did not accept the app's key. Restart Aqua and try again.";
  if (status === 403) return 'Only the Aqua app can do this. Open it from the Aqua window, not from a script or another program.';
  if (status === 429) return 'Aqua asks for a pause. Try again in a moment.';
  if (serverMessage !== '') return tidy(serverMessage);
  if (status === 404 && wording.notFound) return wording.notFound;
  if (status === 503) return 'Aqua is busy or still starting. Try again in a moment.';
  if (status === 504) return 'Aqua took too long to answer. Try again in a moment.';
  return wording.fallback;
}

/** What to show for anything that was thrown: a sentence we wrote, or a plain one for a failure nobody planned for. */
export function errorSentence(error: unknown): string {
  return error instanceof PlainError ? error.message : NOT_DONE;
}

/** True when the answer was a refusal about the app's key (401 or 403): nothing the person does on the page helps. */
export function isKeyProblem(error: unknown): boolean {
  return error instanceof PlainError && (error.status === 401 || error.status === 403);
}

/** One answer of the core: its status, and the body when it was JSON (null when it was not). */
export interface Answer {
  status: number;
  ok: boolean;
  body: unknown;
}

/** Makes the call. Only "no answer at all" throws (a PlainError with a sentence); a not-ok answer is handed back for the caller to read. */
export async function sendRequest(url: string, init?: RequestInit): Promise<Answer> {
  let response: Response;
  try {
    response = await fetch(url, init);
  } catch {
    throw new PlainError(NO_ANSWER);
  }
  let body: unknown = null;
  try {
    body = await response.json();
  } catch {
    /* not JSON: the status is all there is */
  }
  return { status: response.status, ok: response.ok, body };
}

/** A not-ok answer as the error to throw. */
export function failure(answer: Answer, wording: Wording): PlainError {
  return new PlainError(failureSentence(answer.status, serverSentence(answer.body), wording), answer.status);
}

/** The request init for a JSON body. */
export function jsonInit(method: string, body?: unknown): RequestInit {
  if (body === undefined) return { method };
  return { method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) };
}

/** A whole number of at least 0, or 0. */
export function count(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

export function asText(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

/**
 * Text that comes from outside, ready to be shown as it is. Anything that takes up no room or changes how the text around it is drawn
 * (a right-to-left override that turns `evil.exe` into `exe.live`, a zero-width character in the middle of a name, a line break that starts
 * a line of its own and reads like part of the question) is shown by its number instead, as the core does for the questions it builds
 * (`aqua_tool_host::explain::shown_whole`). A tab and a line break become a visible mark.
 */
export function visibleText(value: string, keepBreaks = false): string {
  let out = '';
  for (const ch of value) {
    const cp = ch.codePointAt(0) ?? 0;
    // For a block that is drawn with its line breaks (what a tool returned), a break and a tab stay; everything else invisible is still marked.
    if (keepBreaks && (ch === '\n' || ch === '\t')) out += ch;
    else if (keepBreaks && ch === '\r') continue;
    else if (ch === '\n' || ch === '\r') out += '⏎';
    else if (ch === '\t') out += '→';
    else if (isInvisible(cp)) out += `‹U+${cp.toString(16).toUpperCase().padStart(4, '0')}›`;
    else out += ch;
  }
  return out;
}

function isInvisible(cp: number): boolean {
  return (
    cp < 0x20 ||
    (cp >= 0x7f && cp <= 0x9f) ||
    cp === 0xad ||
    cp === 0x34f ||
    cp === 0x61c ||
    cp === 0x115f ||
    cp === 0x1160 ||
    cp === 0x17b4 ||
    cp === 0x17b5 ||
    (cp >= 0x180b && cp <= 0x180f) ||
    (cp >= 0x200b && cp <= 0x200f) ||
    (cp >= 0x2028 && cp <= 0x202e) ||
    (cp >= 0x2060 && cp <= 0x206f) ||
    cp === 0x3164 ||
    (cp >= 0xfe00 && cp <= 0xfe0f) ||
    cp === 0xfeff ||
    cp === 0xffa0 ||
    (cp >= 0xfff9 && cp <= 0xfffb) ||
    (cp >= 0xe0000 && cp <= 0xe0fff)
  );
}

/** `visibleText` for a value that may be anything: a string is made visible, everything else is ''. */
export function shownText(value: unknown, keepBreaks = false): string {
  return typeof value === 'string' ? visibleText(value, keepBreaks) : '';
}

/** The text cut to `max` characters (not UTF-16 units) with an ellipsis, on one line. */
export function cutText(value: string, max: number): string {
  const letters = Array.from(value);
  return letters.length > max ? `${letters.slice(0, Math.max(0, max - 1)).join('')}…` : value;
}
