// The vault: the folders Aqua reads so that it can answer from the person's own files and saved pages (section 5.5 of
// documentation/UI_CONTRACT.md; core/src/vault/mod.rs). Every route here needs the UI token, which the shell or the dev proxy adds.
//
//   GET  /api/v1/vault                  which folders are read, how much is in it, whether it is paused or being read, how the last reading went
//   GET  /api/v1/vault/search?q=&limit= a keyword search of what was read; the passages are text out of the person's files
//   POST /api/v1/vault/folders          {path} adds a folder (nothing is read until it is indexed)
//   POST /api/v1/vault/folders/remove   {path} takes a folder off and forgets everything read from it, for real
//   POST /api/v1/vault/index            reads the folders in the background (202), or 409 when it is being read already
//   POST /api/v1/vault/pause            {paused} pauses or resumes the reading
//   POST /api/v1/vault/forget           {confirm: "forget everything"} forgets everything read, for real
//
// This vault holds no secret and no value: it is an index of documents. What it can show of them is the passages `search` returns, and
// only `search` returns any text from a file: every other function here gives back counts, folder names and sentences. Nothing in this
// module logs, stores or keeps an answer; the page holds the passages in its state for a minute at most (RESULTS_VISIBLE_SECONDS).

import { answerBody, callCore, count, CoreError, isObject, jsonInit, stringList, UNREADABLE_ANSWER, type Wording } from './coreRequest.ts';

/** What the person has to type before everything is forgotten. The same words the route asks for. */
export const FORGET_PHRASE = 'forget everything';
/** How long found passages stay on the page. They are text out of the person's files, so they are not left on the screen. */
export const RESULTS_VISIBLE_SECONDS = 60;
/** The most passages asked for (the core allows 20). */
export const SEARCH_LIMIT = 5;
const MAX_QUERY_CHARS = 500;
const MAX_PATH_CHARS = 1000;

/** How the last reading of the folders went (counts and names only). */
export interface Reading {
  indexed: number;
  unchanged: number;
  removed: number;
  skippedSecret: number;
  skippedOther: number;
  /** Files that could not be read (names). */
  failed: string[];
  paused: boolean;
  /** The reading stopped at the limit of files for one run; the next one goes on. */
  truncated: boolean;
  /** Folders (or parts) that could not be listed: a drive that is not there, a share that is offline. */
  unlisted: string[];
}

export interface VaultState {
  documents: number;
  passages: number;
  folders: string[];
  paused: boolean;
  /** A reading is going now. */
  indexing: boolean;
  /** Null when no reading has finished since Aqua started. */
  lastReading: Reading | null;
}

/** A stretch of a file, as the search found it. */
export interface Passage {
  path: string;
  /** Where in the file: `page 3`, `slide 2`, `sheet Sales`, `part 1`. */
  where: string;
  /** The text itself, as it is in the file. */
  quote: string;
  /** `file` or `saved page`. */
  source: string;
}

export interface SearchResult {
  passages: Passage[];
  /** The core's note on what the search is (a keyword search). */
  note: string;
}

export function mapReading(raw: unknown): Reading | null {
  if (!isObject(raw)) return null;
  return {
    indexed: count(raw.indexed),
    unchanged: count(raw.unchanged),
    removed: count(raw.removed),
    skippedSecret: count(raw.skipped_secret),
    skippedOther: count(raw.skipped_other),
    failed: stringList(raw.failed),
    paused: raw.paused === true,
    truncated: raw.truncated === true,
    unlisted: stringList(raw.unlisted),
  };
}

/** The vault as the page shows it, or null when the answer is not the vault's status at all. A field that is missing is a count of zero, an empty list, or "no". */
export function mapVault(body: unknown): VaultState | null {
  if (!isObject(body) || !isObject(body.stats)) return null;
  const stats = body.stats;
  return {
    documents: count(stats.docs),
    passages: count(stats.chunks),
    folders: stringList(stats.folders),
    paused: stats.paused === true,
    indexing: body.indexing === true,
    lastReading: mapReading(body.last_index),
  };
}

/** A passage, or null when it holds no text. */
export function mapPassage(raw: unknown): Passage | null {
  if (!isObject(raw) || typeof raw.quote !== 'string' || raw.quote.trim() === '') return null;
  return {
    path: typeof raw.path === 'string' ? raw.path : '',
    where: typeof raw.loc === 'string' ? raw.loc : '',
    quote: raw.quote,
    source: typeof raw.source === 'string' ? raw.source : 'file',
  };
}

/** The answer of the search route, or null when it is not a list of passages. Passages are in the order the core ranked them. */
export function mapSearch(body: unknown): SearchResult | null {
  if (!isObject(body) || !Array.isArray(body.passages)) return null;
  const passages: Passage[] = [];
  for (const row of body.passages) {
    const p = mapPassage(row);
    if (p) passages.push(p);
  }
  return { passages, note: typeof body.note === 'string' ? body.note.trim() : '' };
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

export function documentsText(n: number): string {
  return n === 1 ? '1 document' : `${n.toLocaleString('en-US')} documents`;
}

export function passagesText(n: number): string {
  return n === 1 ? '1 passage' : `${n.toLocaleString('en-US')} passages`;
}

const MAX_LISTED = 3;

function someOf(names: string[]): string {
  const shown = names.slice(0, MAX_LISTED).join(', ');
  return names.length > MAX_LISTED ? `${shown} and ${names.length - MAX_LISTED} more` : shown;
}

/** How a reading went, one line each. Empty when nothing is worth saying. */
export function readingLines(r: Reading): string[] {
  const lines = [`${r.indexed} read or updated, ${r.unchanged} unchanged.`];
  if (r.removed > 0) lines.push(`${r.removed} forgotten because they are gone from the folders.`);
  if (r.skippedSecret > 0) lines.push(`${r.skippedSecret} not read because the name looks like a password or a key.`);
  if (r.skippedOther > 0) lines.push(`${r.skippedOther} not read (a link, a file over 25 MB, or a kind Aqua does not read).`);
  if (r.failed.length > 0) lines.push(`${r.failed.length === 1 ? '1 file' : `${r.failed.length} files`} could not be read: ${someOf(r.failed)}.`);
  if (r.unlisted.length > 0) lines.push(`Could not look in: ${someOf(r.unlisted)}. What was read there before is kept.`);
  if (r.truncated) lines.push('It stopped at the limit for one reading. The next reading goes on from there.');
  if (r.paused) lines.push('It stopped because reading is paused.');
  return lines;
}

/** One sentence for what the vault is doing now. */
export function statusText(state: VaultState): string {
  if (state.indexing) return 'Reading the folders now. It waits while you are talking to Aqua.';
  if (state.paused) return 'Reading is paused. Nothing is read until you resume.';
  if (state.folders.length === 0) return 'No folder is read. Add a folder to let Aqua answer from your files.';
  if (state.lastReading === null) return 'The folders have not been read since Aqua started.';
  return 'Up to date as of the last reading.';
}

/** Where a passage is from, as a line: the file and the place in it. */
export function passageWhere(p: Passage): string {
  const place = p.where.trim();
  const from = p.path.trim() === '' ? 'unknown file' : p.path.trim();
  return place === '' ? from : `${from} · ${place}`;
}

/** What a search needs before it is sent: the trimmed words, or the sentence that says why not. */
export function checkQuery(text: string): { ok: true; query: string } | { ok: false; sentence: string } {
  const q = text.trim();
  if (q === '') return { ok: false, sentence: 'Type a few words to look for.' };
  if (Array.from(q).length > MAX_QUERY_CHARS) return { ok: false, sentence: `Use fewer words (at most ${MAX_QUERY_CHARS} characters).` };
  return { ok: true, query: q };
}

/** What a folder needs before it is sent: the trimmed path, or the sentence that says why not. */
export function checkFolder(text: string): { ok: true; path: string } | { ok: false; sentence: string } {
  const p = text.trim();
  if (p === '') return { ok: false, sentence: 'Type the full path of a folder, for example C:\\Users\\you\\Documents.' };
  if (Array.from(p).length > MAX_PATH_CHARS) return { ok: false, sentence: 'That path is too long.' };
  return { ok: true, path: p };
}

/** Whether what the person typed is the phrase that confirms forgetting everything (case and the spaces around it do not matter). */
export function confirmsForgetting(typed: string): boolean {
  return typed.trim().toLowerCase() === FORGET_PHRASE;
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

const NOT_OPEN = 'The vault is not open.';
const STATE: Wording = { fallback: 'The vault could not be read.', status: { 503: NOT_OPEN } };
const SEARCH: Wording = { fallback: 'The search did not work.', status: { 503: NOT_OPEN } };
const ADD: Wording = { fallback: 'The folder could not be added.', status: { 503: NOT_OPEN } };
const REMOVE: Wording = { fallback: 'The folder could not be taken off.', status: { 503: NOT_OPEN } };
const INDEX: Wording = { fallback: 'Aqua could not start reading the folders.', status: { 503: NOT_OPEN, 409: 'The folders are being read already.' } };
const PAUSE: Wording = { fallback: 'Reading could not be paused or resumed.', status: { 503: NOT_OPEN } };
const FORGET: Wording = { fallback: 'The vault could not be emptied.', status: { 503: NOT_OPEN } };

export const vaultApi = {
  /** The status of the vault: counts, folder names and sentences. No text from a file. */
  state: async (): Promise<VaultState> => {
    const state = mapVault(answerBody(await callCore('/api/v1/vault', undefined, STATE)));
    if (!state) throw new CoreError(UNREADABLE_ANSWER);
    return state;
  },
  /** A keyword search of what was read. The only call that gives back text out of a file; the person asked for it by searching. */
  search: async (text: string): Promise<SearchResult> => {
    const checked = checkQuery(text);
    if (!checked.ok) throw new CoreError(checked.sentence, 400);
    const url = `/api/v1/vault/search?q=${encodeURIComponent(checked.query)}&limit=${SEARCH_LIMIT}`;
    const result = mapSearch(answerBody(await callCore(url, undefined, SEARCH)));
    if (!result) throw new CoreError(UNREADABLE_ANSWER);
    return result;
  },
  /** Adds a folder to read. Gives the folder as the core wrote it. Nothing is read until `startReading`. */
  addFolder: async (text: string): Promise<{ folder: string }> => {
    const checked = checkFolder(text);
    if (!checked.ok) throw new CoreError(checked.sentence, 400);
    const body = answerBody(await callCore('/api/v1/vault/folders', jsonInit('POST', { path: checked.path }), ADD));
    return { folder: isObject(body) && typeof body.folder === 'string' ? body.folder : checked.path };
  },
  /** Takes a folder off and forgets everything read from it. `note` is set when the file could not be tidied afterwards. */
  removeFolder: async (path: string): Promise<{ forgotten: number; note: string | null }> => {
    if (typeof path !== 'string' || path.trim() === '') throw new CoreError(REMOVE.fallback, 400);
    const body = answerBody(await callCore('/api/v1/vault/folders/remove', jsonInit('POST', { path }), REMOVE));
    const tidyFailed = isObject(body) && typeof body.compact_error === 'string' && body.compact_error !== '';
    return {
      forgotten: isObject(body) ? count(body.forgotten_documents) : 0,
      note: tidyFailed ? 'The documents are forgotten. The file could not be tidied just now; Aqua tries again the next time something is forgotten.' : null,
    };
  },
  /** Starts reading the folders in the background. `started` is false when they are being read already. */
  startReading: async (): Promise<{ started: boolean }> => {
    const answer = await callCore('/api/v1/vault/index', jsonInit('POST'), INDEX);
    if (!answer.ok && answer.status === 409) return { started: false };
    answerBody(answer);
    return { started: true };
  },
  /** Pauses or resumes the reading. Gives back what the core says is so now. */
  setPaused: async (paused: boolean): Promise<boolean> => {
    const body = answerBody(await callCore('/api/v1/vault/pause', jsonInit('POST', { paused }), PAUSE));
    if (!isObject(body) || typeof body.paused !== 'boolean') throw new CoreError(UNREADABLE_ANSWER);
    return body.paused;
  },
  /** Forgets everything that was read, for real. The person has to have typed the phrase; the route is never asked without it. */
  forgetAll: async (typed: string): Promise<{ forgotten: number }> => {
    if (!confirmsForgetting(typed)) throw new CoreError(`To forget everything, type “${FORGET_PHRASE}”.`, 400);
    const body = answerBody(await callCore('/api/v1/vault/forget', jsonInit('POST', { confirm: FORGET_PHRASE }), FORGET));
    return { forgotten: isObject(body) ? count(body.forgotten_documents) : 0 };
  },
};
