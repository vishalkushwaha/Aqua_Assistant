// What Aqua remembers about the person, and what its memory suggests (section 5.4 of documentation/UI_CONTRACT.md). Every route here
// needs the UI token, which the shell or the dev proxy adds. The page keeps nothing of its own: facts and suggestions live in the core,
// and a screen shows what the core last said. Text from the core (a fact, a reason) is only ever handed to React as a text child.
//
//   GET    /api/v1/memory/facts                      every fact, and whether Aqua is incognito
//   POST   /api/v1/memory/facts {text}               remembers a fact (a secret is refused with the reason)
//   DELETE /api/v1/memory/facts/{id}                 forgets one fact
//   DELETE /api/v1/memory/facts                      forgets everything (the only door to it)
//   POST   /api/v1/memory/incognito {on}             stops remembering for now
//   GET    /api/v1/memory/proposals?status=pending   what the memory suggests
//   POST   /api/v1/memory/proposals/{id}/accept      yes to a suggestion
//   POST   /api/v1/memory/proposals/{id}/reject      no to a suggestion: it changes nothing and is not made again
//   POST   /api/v1/memory/dream                      looks through what was remembered now and queues what it finds
//
// There is no route that edits a fact or searches the facts, so the page has neither: a fact is forgotten and said again.

import { agoText } from './recipes.ts';
import { answerBody, callCore, count, CoreError, isObject, isWholeNumber, jsonInit, UNREADABLE_ANSWER, unixOrNull, type Wording } from './coreRequest.ts';

/** The longest a fact may be, in characters (the core refuses more: core/src/memory/user_facts/store.rs `MAX_CHARS`). */
export const MAX_FACT_CHARS = 300;

export interface Fact {
  id: number;
  /** `fact`, `preference` or `lesson`; anything else is shown as a fact. */
  kind: string;
  text: string;
  /** Who wrote it: `user` (their own words), `ui` (the app) or `tool` (a tool call the person approved). */
  source: string;
  /** Seconds since 1970, or null when the core does not say. */
  created: number | null;
  /** Seconds since 1970 of the last time Aqua used it, or null when it never did. */
  lastUsed: number | null;
  uses: number;
  /** About health, money, papers and the like: only shown to the person, never put into a prompt on its own. */
  sensitive: boolean;
}

export interface MemoryFacts {
  /** Newest first. */
  facts: Fact[];
  incognito: boolean;
}

export interface Suggestion {
  id: number;
  /** `merge`, `dates` or `lesson`; another word is shown as a plain suggestion. */
  kind: string;
  /** The facts it is about: what a merge forgets, the one a date change rewrites. Empty for a lesson. */
  factIds: number[];
  /** The fact a merge keeps. */
  keepId: number | null;
  /** The core's own words for why it suggests this. */
  reason: string;
  /** What the fact will say once the suggestion is accepted. */
  newText: string;
  created: number | null;
  /** `pending`, `accepted` or `rejected`. */
  status: string;
}

export interface Suggestions {
  /** Newest first. */
  suggestions: Suggestion[];
}

/** One fact, or null when it is not one the page could forget (no whole-number id). A field that is missing becomes a plain default. */
export function mapFact(raw: unknown): Fact | null {
  if (!isObject(raw) || !isWholeNumber(raw.id)) return null;
  return {
    id: raw.id,
    kind: typeof raw.kind === 'string' ? raw.kind : 'fact',
    text: typeof raw.text === 'string' ? raw.text : '',
    source: typeof raw.source === 'string' ? raw.source : '',
    created: unixOrNull(raw.created),
    lastUsed: unixOrNull(raw.last_used),
    uses: count(raw.uses),
    sensitive: raw.sensitive === true,
  };
}

/** The answer of the list route: newest first, or null when it is not a list at all. */
export function mapFacts(body: unknown): MemoryFacts | null {
  if (!isObject(body) || !Array.isArray(body.facts)) return null;
  const facts: Fact[] = [];
  for (const row of body.facts) {
    const fact = mapFact(row);
    if (fact) facts.push(fact);
  }
  facts.sort((a, b) => (b.created ?? 0) - (a.created ?? 0) || b.id - a.id);
  return { facts, incognito: body.incognito === true };
}

/** One suggestion, or null when it has no whole-number id (it could not be answered). */
export function mapSuggestion(raw: unknown): Suggestion | null {
  if (!isObject(raw) || !isWholeNumber(raw.id)) return null;
  return {
    id: raw.id,
    kind: typeof raw.kind === 'string' ? raw.kind : '',
    factIds: Array.isArray(raw.fact_ids) ? raw.fact_ids.filter(isWholeNumber) : [],
    keepId: isWholeNumber(raw.keep_id) ? raw.keep_id : null,
    reason: typeof raw.reason === 'string' ? raw.reason : '',
    newText: typeof raw.new_text === 'string' ? raw.new_text : '',
    created: unixOrNull(raw.created),
    // Asked for with status=pending: a row that does not say is one the core listed as waiting.
    status: typeof raw.status === 'string' ? raw.status : 'pending',
  };
}

/** The waiting suggestions, oldest first, so that they are answered in the order they were made; or null when the answer is not a list. */
export function mapSuggestions(body: unknown): Suggestions | null {
  if (!isObject(body) || !Array.isArray(body.proposals)) return null;
  const suggestions: Suggestion[] = [];
  for (const row of body.proposals) {
    const s = mapSuggestion(row);
    if (s && s.status === 'pending') suggestions.push(s);
  }
  suggestions.sort((a, b) => (a.created ?? 0) - (b.created ?? 0) || a.id - b.id);
  return { suggestions };
}

// ---- wording ---------------------------------------------------------------------------------------------------------------------------

/** Who wrote a fact, in words. */
export function sourceText(source: string): string {
  if (source === 'user') return 'you said it';
  if (source === 'ui') return 'added in the app';
  if (source === 'tool') return 'saved by a tool you approved';
  return 'source not recorded';
}

export function kindText(kind: string): string {
  if (kind === 'preference') return 'preference';
  if (kind === 'lesson') return 'lesson from a correction';
  return 'fact';
}

/** What a fact's text shows: the text, or a stand-in when the core sent none. */
export function factText(text: string): string {
  const t = text.trim();
  return t === '' ? '(no text)' : t;
}

export const PRIVATE_HINT = 'About health, money, papers or the like: shown only here, never put into a prompt on its own.';

/** The one line under a fact: what kind, when it was learned, where from, how it was used. `nowSecs` is given so the answer does not depend on the clock. */
export function factLine(fact: Fact, nowSecs: number, formatDate?: (unix: number) => string): string {
  const parts = [kindText(fact.kind)];
  if (fact.created !== null) parts.push(`learned ${agoText(fact.created, nowSecs, formatDate)}`);
  parts.push(sourceText(fact.source));
  if (fact.uses > 0) parts.push(fact.uses === 1 ? 'used once' : `used ${fact.uses} times`);
  if (fact.lastUsed !== null) parts.push(`last used ${agoText(fact.lastUsed, nowSecs, formatDate)}`);
  return parts.join(' · ');
}

/** The facts as the suggestions need them: by id. */
export function factsById(facts: Fact[]): Map<number, Fact> {
  return new Map(facts.map((f) => [f.id, f]));
}

export interface SuggestionWords {
  title: string;
  /** What accepting would do, one short line each. */
  lines: string[];
  /** The core's reason, or '' when it gave none. */
  because: string;
}

function quoted(text: string): string {
  return `“${factText(text)}”`;
}

/** The words for one suggestion: what it is, and exactly what "Remember" would change. `facts` are the facts the page knows, by id. */
export function describeSuggestion(s: Suggestion, facts: Map<number, Fact>): SuggestionWords {
  const because = s.reason.trim();
  if (s.kind === 'merge') {
    const forgotten = s.factIds.map((id) => facts.get(id)).filter((f): f is Fact => f !== undefined);
    const lines = [`Keep: ${quoted(s.newText)}`];
    if (forgotten.length > 0) for (const f of forgotten) lines.push(`Forget the copy: ${quoted(f.text)}`);
    else lines.push(s.factIds.length === 1 ? 'Forget one other fact that says the same.' : `Forget ${s.factIds.length} other facts that say the same.`);
    return { title: 'The same thing is remembered twice', lines, because };
  }
  if (s.kind === 'dates') {
    const now = s.factIds.length > 0 ? facts.get(s.factIds[0]) : undefined;
    const lines: string[] = [];
    if (now) lines.push(`Now it says: ${quoted(now.text)}`);
    lines.push(`It would say: ${quoted(s.newText)}`);
    return { title: 'A date that no longer means anything', lines, because };
  }
  if (s.kind === 'lesson') {
    return { title: 'You corrected an answer', lines: [`Remember: ${quoted(s.newText)}`], because };
  }
  return { title: 'A suggestion', lines: s.newText.trim() === '' ? [] : [`It would say: ${quoted(s.newText)}`], because };
}

/** What each button of a suggestion does, so that the page can say it. */
export const REMEMBER_HINT = 'Remember: makes the change above.';
export const NOT_THIS_HINT = 'Not this: changes nothing, and Aqua will not suggest it again.';

/** The sentence for what the page is told after the person answered a suggestion. */
export function decidedSentence(accepted: boolean): string {
  return accepted ? 'Done. Aqua made that change.' : 'Done. Aqua will not suggest that again.';
}

/** What a typed fact needs before it is sent: the trimmed text, or the sentence that says why not. */
export function checkFactText(text: string): { ok: true; text: string } | { ok: false; sentence: string } {
  const t = text.trim();
  if (t === '') return { ok: false, sentence: 'Type what you want Aqua to remember.' };
  if (Array.from(t).length > MAX_FACT_CHARS) return { ok: false, sentence: `That is too long to remember (at most ${MAX_FACT_CHARS} characters).` };
  return { ok: true, text: t };
}

// ---- the calls -------------------------------------------------------------------------------------------------------------------------

const LIST: Wording = { fallback: 'What Aqua remembers could not be read.', status: { 503: 'The memory could not be opened.' } };
const SUGGESTIONS: Wording = { fallback: 'The suggestions could not be read.', status: { 503: 'The memory could not be opened.' } };
const ADD: Wording = { fallback: 'That could not be remembered.' };
const FORGET: Wording = { fallback: 'That could not be forgotten.' };
const FORGET_ALL: Wording = { fallback: 'The memory could not be cleared.' };
const INCOGNITO: Wording = { fallback: 'The incognito switch could not be changed.' };
const DECIDE: Wording = {
  fallback: 'That suggestion could not be answered.',
  status: {
    404: 'That suggestion is gone.',
    409: 'That suggestion was decided already.',
    422: 'That suggestion can no longer be applied, because the facts it was about have changed.',
  },
};
const DREAM: Wording = { fallback: 'Aqua could not look through what it remembers.' };

export const memoryApi = {
  /** Everything remembered (private facts included: it is the person's own list), and whether Aqua is incognito. */
  facts: async (): Promise<MemoryFacts> => {
    const list = mapFacts(answerBody(await callCore('/api/v1/memory/facts', undefined, LIST)));
    if (!list) throw new CoreError(UNREADABLE_ANSWER);
    return list;
  },
  /** Remembers a fact. A text that is empty or too long never reaches the core; a secret is refused by the core, with its reason. */
  remember: async (text: string): Promise<void> => {
    const checked = checkFactText(text);
    if (!checked.ok) throw new CoreError(checked.sentence, 400);
    answerBody(await callCore('/api/v1/memory/facts', jsonInit('POST', { text: checked.text }), ADD));
  },
  /** Forgets one fact. `forgotten` is false when there was no such fact: it is gone already. */
  forget: async (id: number): Promise<{ forgotten: boolean }> => {
    if (!isWholeNumber(id)) throw new CoreError(FORGET.fallback, 400);
    const answer = await callCore(`/api/v1/memory/facts/${id}`, jsonInit('DELETE'), FORGET);
    if (!answer.ok && answer.status === 404) return { forgotten: false };
    answerBody(answer);
    return { forgotten: true };
  },
  /** Forgets every fact. The page asks first. */
  forgetAll: async (): Promise<{ deleted: number }> => {
    const body = answerBody(await callCore('/api/v1/memory/facts', jsonInit('DELETE'), FORGET_ALL));
    return { deleted: isObject(body) ? count(body.deleted) : 0 };
  },
  /** Stops (or goes on) remembering. The answer is what the core says is now so; a page shows that, not what it asked for. */
  setIncognito: async (on: boolean): Promise<boolean> => {
    const body = answerBody(await callCore('/api/v1/memory/incognito', jsonInit('POST', { on }), INCOGNITO));
    if (!isObject(body) || typeof body.incognito !== 'boolean') throw new CoreError(UNREADABLE_ANSWER);
    return body.incognito;
  },
  /** The suggestions that wait, oldest first. */
  suggestions: async (): Promise<Suggestions> => {
    const list = mapSuggestions(answerBody(await callCore('/api/v1/memory/proposals?status=pending', undefined, SUGGESTIONS)));
    if (!list) throw new CoreError(UNREADABLE_ANSWER);
    return list;
  },
  /** The person's answer to one suggestion. */
  decide: async (id: number, accept: boolean): Promise<void> => {
    if (!isWholeNumber(id)) throw new CoreError(DECIDE.fallback, 400);
    answerBody(await callCore(`/api/v1/memory/proposals/${id}/${accept ? 'accept' : 'reject'}`, jsonInit('POST'), DECIDE));
  },
  /** Looks through what was remembered now. It changes no fact; it says in one sentence what it found. */
  lookForSuggestions: async (): Promise<string> => {
    const body = answerBody(await callCore('/api/v1/memory/dream', jsonInit('POST'), DREAM));
    return isObject(body) && typeof body.summary === 'string' && body.summary.trim() !== '' ? body.summary.trim() : 'Aqua looked through what it remembers.';
  },
};
