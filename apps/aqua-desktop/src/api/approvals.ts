// The questions that wait for the person's answer (GET /api/v1/browser/confirmations, answered with POST /api/v1/browser/confirm; both need
// the UI token, which the shell or the dev proxy adds, so these are plain relative fetches). The same data arrives as the `approval` event of
// a streamed turn (documentation/UI_CONTRACT.md section 3): `mapApproval` reads either.
//
// A question is the exact action and its target, what it can do, the rule that made it ask, what the run has read and holds, and the moment
// after which an answer is too late. Everything here is a plain function, so a test can call it without a page. Text from the core is only
// ever handed to React as a text child, and is made visible first (see `visibleText`): a question must show what is really there.

import { asText, failure, failureSentence, isObject, jsonInit, PlainError, sendRequest, serverSentence, shownText, UNREADABLE_ANSWER, type Answer as HttpAnswer } from './plainErrors.ts';

export type ApprovalKind = 'site' | 'action' | 'tool';

/** The three answers the core accepts (documentation/UI_CONTRACT.md section 3). Nothing else is ever sent. */
export type ApprovalAnswer = 'approve' | 'approve_always' | 'deny';

export interface ApprovalQuestion {
  requestId: string;
  /** `site`: an address that is not known. `action`: a send, a payment, a password field, an upload. `tool`: a desktop, file or command tool. An unknown kind is read as `action`, the strictest. */
  kind: ApprovalKind;
  tool: string;
  /** The exact action, as the core words it ("Run a command: git status"). */
  reason: string;
  url: string | null;
  site: string | null;
  createdMs: number | null;
  /** Unix milliseconds after which an answer is too late, or null when the question carries no time. */
  expiresMs: number | null;
  /** Asked every time: the answer is never remembered, so "always" is never offered. */
  alwaysAsk: boolean;
  /** What the call can do (`WRITE`, `EXECUTE`, ...). */
  classes: string[];
  /** One sentence on what could go wrong. */
  risk: string;
  /** The rule that made it ask, in the policy's words. */
  rule: string;
  /** What is involved: the target, and what the run holds that could leave or could be steering it. */
  data: string[];
  runId: string | null;
  /** The question carries the explanation (risk, rule or data). An older core, or a question of the old routes, does not: the card then shows the plain text. */
  explained: boolean;
  /**
   * Only when the core offers the run option (card OA6-05, `AQUA_EDIT_GRANT=on`): the sentence that says what it allows. The key is left out of every other question,
   * so that a question without it is read exactly as it was. The card shows it as text and offers the fourth answer only through `runOptionFor`.
   */
  runGrant?: string;
}

const MAX_CLASSES = 12;
const MAX_DATA = 10;

function wholeMs(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? Math.floor(value) : null;
}

function strings(value: unknown, max: number): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const item of value) {
    if (typeof item === 'string' && item.trim() !== '') out.push(item);
    if (out.length >= max) break;
  }
  return out;
}

/**
 * One question, or null when it has no request id (nothing could be answered). A missing field becomes the plain default. A kind the page
 * does not know is read as `action`: the one that is never remembered. The moment an answer is too late is `expires_unix_ms`; a core that
 * does not send it (older) gives `created_unix_ms + expires_in_secs`.
 */
export function mapApproval(raw: unknown): ApprovalQuestion | null {
  if (!isObject(raw)) return null;
  const id = raw.request_id;
  if (typeof id !== 'string' || id.trim() === '') return null;
  const kind: ApprovalKind = raw.kind === 'site' || raw.kind === 'tool' ? raw.kind : 'action';
  const createdMs = wholeMs(raw.created_unix_ms);
  const secs = typeof raw.expires_in_secs === 'number' && Number.isFinite(raw.expires_in_secs) && raw.expires_in_secs >= 0 ? raw.expires_in_secs : null;
  const expiresMs = wholeMs(raw.expires_unix_ms) ?? (createdMs !== null && secs !== null ? createdMs + Math.floor(secs * 1000) : null);
  const risk = shownText(raw.risk);
  const rule = shownText(raw.rule);
  const data = strings(raw.data, MAX_DATA).map((d) => shownText(d));
  const site = shownText(raw.site);
  const url = shownText(raw.url);
  const runId = asText(raw.run_id).trim();
  // Only a sentence with something in it counts as an offer (judged before the invisible is made visible: a line break alone is no sentence); the key is not there at all otherwise.
  const grant = typeof raw.run_grant === 'string' && raw.run_grant.trim() !== '' ? shownText(raw.run_grant) : '';
  return {
    requestId: id,
    kind,
    tool: shownText(raw.tool),
    reason: shownText(raw.reason),
    url: url !== '' ? url : null,
    site: site !== '' ? site : null,
    createdMs,
    expiresMs,
    alwaysAsk: raw.always_ask === true,
    classes: strings(raw.classes, MAX_CLASSES).map((c) => shownText(c)),
    risk,
    rule,
    data,
    runId: runId !== '' ? runId : null,
    explained: risk !== '' || rule !== '' || data.length > 0,
    ...(grant !== '' ? { runGrant: grant } : {}),
  };
}

/** The list route's answer (`{"pending": [...]}`), oldest question first, or null when it is not a list at all. A row that is not a question is left out. */
export function mapApprovalList(body: unknown): ApprovalQuestion[] | null {
  if (!isObject(body) || !Array.isArray(body.pending)) return null;
  const out: ApprovalQuestion[] = [];
  for (const row of body.pending) {
    const q = mapApproval(row);
    if (q) out.push(q);
  }
  return out.sort((a, b) => (a.createdMs ?? Number.MAX_SAFE_INTEGER) - (b.createdMs ?? Number.MAX_SAFE_INTEGER) || (a.requestId < b.requestId ? -1 : a.requestId > b.requestId ? 1 : 0));
}

// ---- the time left ------------------------------------------------------------------------------

/** Whole seconds left before an answer is too late (rounded up, so it shows 1 until the moment has passed), or null when the question carries no time. */
export function secondsLeft(q: ApprovalQuestion, nowMs: number): number | null {
  if (q.expiresMs === null) return null;
  return Math.max(0, Math.ceil((q.expiresMs - nowMs) / 1000));
}

/** An answer now would be too late. A question with no time is never too late here: the core decides. */
export function isTooLate(q: ApprovalQuestion, nowMs: number): boolean {
  return q.expiresMs !== null && nowMs >= q.expiresMs;
}

export function secondsText(seconds: number): string {
  return seconds === 1 ? '1 second' : `${seconds} seconds`;
}

export const TOO_LATE_SENTENCE = 'Too late to answer. Nobody answered in time, so Aqua told the run no and nothing was done.';

/** The line under the question: how long the agent waits, or that it is too late. */
export function waitingSentence(q: ApprovalQuestion, nowMs: number): string {
  if (isTooLate(q, nowMs)) return TOO_LATE_SENTENCE;
  const left = secondsLeft(q, nowMs);
  if (left === null) return 'The agent is waiting for your answer.';
  return `The agent is waiting. If you do not answer in ${secondsText(left)}, Aqua denies it.`;
}

// ---- what can be answered -----------------------------------------------------------------------

/** Whether "always" (remember the site, or approve for the session) may be offered: not for an action, and not when the question is asked every time. */
export function allowsAlways(q: ApprovalQuestion): boolean {
  return q.kind !== 'action' && !q.alwaysAsk;
}

/** The answers to offer, in the order of the buttons. `approve_always` is there only where the question allows it. */
export function choicesFor(q: ApprovalQuestion): ApprovalAnswer[] {
  return allowsAlways(q) ? ['deny', 'approve_always', 'approve'] : ['deny', 'approve'];
}

export function titleFor(q: ApprovalQuestion): string {
  return q.kind === 'site' ? 'Allow this website?' : q.kind === 'tool' ? 'Approve this action?' : 'Approve this browser action?';
}

export function answerLabel(q: ApprovalQuestion, answer: ApprovalAnswer): string {
  if (answer === 'deny') return 'Deny';
  if (answer === 'approve_always') return q.kind === 'site' ? `Always allow ${q.site ?? 'this site'}` : 'Approve for this session';
  return q.kind === 'site' ? 'Allow once' : q.kind === 'tool' ? 'Approve once' : 'Approve';
}

/** One plain sentence on what an answer does, shown under the buttons. */
export function answerHint(q: ApprovalQuestion, answer: ApprovalAnswer): string {
  if (answer === 'deny') return 'Aqua tells the run no. Nothing is done.';
  if (answer === 'approve_always') return q.kind === 'site' ? 'The site stays allowed until you take it back.' : 'The same kind of action goes ahead without asking for this session only.';
  return 'Allows this one request only.';
}

// ---- the run option (card OA6-05) ---------------------------------------------------------------

/** The one scope the core knows: "yes to this edit, and to later edits in the same folder until the run ends". It goes with `approve` only. */
export type ApprovalScope = 'run';

/** What the person is shown for the fourth answer: its label, and the core's own sentence about what it allows. */
export interface RunOption {
  label: string;
  sentence: string;
}

export const RUN_OPTION_LABEL = 'Allow edits in this folder for this run';

/**
 * The sentence is shown whole or the option is not shown: it names the folder and says that later edits there go ahead without asking, and a sentence so long that
 * the card would have to cut it (the folder comes from a path an agent chose) could hide that part. The core's own sentence is about 450 characters and a folder name.
 */
export const RUN_GRANT_MAX = 1200;

/**
 * The fourth answer of a question, or null when it is not offered. It is offered only when the core sent a `run_grant` sentence, for a tool question (the only kind the
 * core makes the offer for), and when the sentence can be shown whole. A question without `run_grant` is answered, and shown, exactly as it always was.
 */
export function runOptionFor(q: ApprovalQuestion): RunOption | null {
  const sentence = q.runGrant;
  if (q.kind !== 'tool' || typeof sentence !== 'string' || sentence.trim() === '' || Array.from(sentence).length > RUN_GRANT_MAX) return null;
  return { label: RUN_OPTION_LABEL, sentence };
}

/** The sentence shown when no explanation came with the question: the old text, kept for an older core. */
export const NO_DESCRIPTION = 'Aqua did not describe this action. Deny it unless you know what it is.';

const CLASS_WORDS: Record<string, string> = {
  READ: 'reads something',
  WRITE: 'changes files or settings',
  EXECUTE: 'runs a program',
  NETWORK: 'uses the internet',
  REMOTE: 'acts on another machine',
  DESTRUCTIVE: 'can delete or overwrite',
  CREDENTIAL: 'uses a stored password or key',
  PRIVILEGED: 'needs administrator access',
  EXTERNAL_EFFECT: 'changes another system (sends, posts, starts)',
};

/** What a class of call means, in words. A class the page does not know is shown as the core wrote it, in lower case. */
export function classWords(cls: string): string {
  return CLASS_WORDS[cls.toUpperCase()] ?? cls.toLowerCase().replace(/_/g, ' ');
}

const SERIOUS = new Set(['DESTRUCTIVE', 'PRIVILEGED', 'CREDENTIAL', 'EXTERNAL_EFFECT', 'EXECUTE', 'REMOTE']);

/** Classes that can change things outside Aqua are marked as serious, so that they are not skimmed past. */
export function isSerious(cls: string): boolean {
  return SERIOUS.has(cls.toUpperCase());
}

// ---- answering ----------------------------------------------------------------------------------

/** What an answer came to. It is never assumed: `ok` is true only when the core said so. */
export type AnswerResult =
  | { ok: true; answer: ApprovalAnswer; sentence: string; /** Only when the core says the run option was taken. */ scope?: ApprovalScope }
  | {
      ok: false;
      sentence: string;
      /** The question cannot be answered any more (answered already, too late, taken back, not there): the card offers only "Close". */
      closed: boolean;
      tooLate: boolean;
    };

const ANSWER_WORDS: ApprovalAnswer[] = ['approve', 'approve_always', 'deny'];

export function successSentence(q: ApprovalQuestion, answer: ApprovalAnswer): string {
  if (answer === 'deny') return 'Denied. Nothing was done.';
  if (answer === 'approve_always') return q.kind === 'site' ? 'Approved. This site stays allowed until you take it back.' : 'Approved for this session.';
  return 'Approved. The run has your answer.';
}

/** What the person is told when the core says the run option was taken: it is a yes to this edit, and what it means for the later ones. */
export const RUN_SUCCESS_SENTENCE = 'Approved. Later edits in that folder go ahead without asking until this run ends, or until it reads anything from outside the project.';

const WORDING = { fallback: 'Your answer could not be sent.', notFound: 'That question is gone. It may be from before Aqua restarted.' };

/**
 * The sentence and the state of a not-ok answer of the confirm route. 404, 409 and 410 close the question; the others leave it to try again.
 * For the run option (`scope`) a 409 is the core saying the switch is off, and a 400 that the question does not offer it: neither takes the question away, so the
 * question stays open and the person can answer it again with a plain approve or deny (a question that was in fact answered already leaves the list by itself).
 */
export function answerFailure(answer: HttpAnswer, scope?: ApprovalScope): Extract<AnswerResult, { ok: false }> {
  const expired = answer.status === 410 && isObject(answer.body) && answer.body.expired === true;
  if (expired) return { ok: false, sentence: TOO_LATE_SENTENCE, closed: true, tooLate: true };
  const closed = answer.status === 404 || answer.status === 410 || (answer.status === 409 && scope === undefined);
  return { ok: false, sentence: failureSentence(answer.status, serverSentence(answer.body), WORDING), closed, tooLate: false };
}

const NOT_OFFERED = 'That answer is not offered for this question.';
const BEING_SENT = 'Your first answer is still being sent. Wait for it.';

export const approvalsApi = {
  /** The questions waiting, oldest first (UI token). Throws a PlainError whose message is a sentence. */
  list: async (): Promise<ApprovalQuestion[]> => {
    const answer = await sendRequest('/api/v1/browser/confirmations');
    if (!answer.ok) throw failure(answer, { fallback: 'The waiting questions could not be read.', notFound: 'This version of Aqua cannot list its questions.' });
    const list = mapApprovalList(answer.body);
    if (!list) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return list;
  },

  /**
   * Answers one question (UI token only; the API token cannot approve anything). An answer the question does not allow is never sent. The result says
   * what became of it: success only when the core says `ok`; too late, answered already and not there are told as such.
   *
   * `scope` is the run option (card OA6-05) and it is the only thing that ever puts a `scope` in the request: `'run'` goes with `approve` and with a question that
   * offers it (`runOptionFor`), and is never sent otherwise. One question has one answer on its way at a time: the same answer given again (a double click) is the
   * one request, and a different one is told to wait.
   */
  answer: (q: ApprovalQuestion, decision: ApprovalAnswer, scope?: ApprovalScope): Promise<AnswerResult> => {
    if (!ANSWER_WORDS.includes(decision) || !choicesFor(q).includes(decision)) return Promise.resolve({ ok: false, sentence: NOT_OFFERED, closed: false, tooLate: false });
    if (scope !== undefined && (scope !== 'run' || decision !== 'approve' || runOptionFor(q) === null)) {
      return Promise.resolve({ ok: false, sentence: NOT_OFFERED, closed: false, tooLate: false });
    }
    const way = `${decision}/${scope ?? ''}`;
    const open = sending.get(q.requestId);
    if (open) return open.way === way ? open.result : Promise.resolve({ ok: false, sentence: BEING_SENT, closed: false, tooLate: false });
    const result = send(q, decision, scope).finally(() => sending.delete(q.requestId));
    sending.set(q.requestId, { way, result });
    return result;
  },
};

/** The answers that are on their way, by request id: the second click of a double click must not become a second request. */
const sending = new Map<string, { way: string; result: Promise<AnswerResult> }>();

async function send(q: ApprovalQuestion, decision: ApprovalAnswer, scope: ApprovalScope | undefined): Promise<AnswerResult> {
  let answer: HttpAnswer;
  try {
    const body = scope === undefined ? { request_id: q.requestId, decision } : { request_id: q.requestId, decision, scope };
    answer = await sendRequest('/api/v1/browser/confirm', jsonInit('POST', body));
  } catch (e) {
    return { ok: false, sentence: e instanceof PlainError ? e.message : WORDING.fallback, closed: false, tooLate: false };
  }
  if (!answer.ok) return answerFailure(answer, scope);
  if (!isObject(answer.body) || answer.body.ok !== true) {
    return { ok: false, sentence: 'Aqua did not confirm your answer. Look at the question again before you answer.', closed: false, tooLate: false };
  }
  const said = answer.body.answer;
  const taken = ANSWER_WORDS.find((w) => w === said) ?? decision;
  // The run option counts as taken only when the core says so: a plain yes is never told as more than it was.
  if (scope === 'run' && taken === 'approve' && answer.body.scope === 'run') return { ok: true, answer: taken, sentence: RUN_SUCCESS_SENTENCE, scope: 'run' };
  return { ok: true, answer: taken, sentence: successSentence(q, taken) };
}

/** What to tell the person when the list cannot be read because of the key (401, 403): a question that needs their yes would not be seen. */
export const QUESTIONS_HIDDEN =
  "Questions from Aqua cannot be shown in this window, so anything that needs your yes will be refused when its time runs out. Open Aqua from its own window.";
