// Saying something to the task that is running (POST /api/v1/turn/steer, UI token; documentation/UI_CONTRACT.md sections 5.3 and 6).
//
// What is said is queued (five lines at most) and the agent reads it at its next step, as the person's own words. "stop" ends the run at once.
// Nothing said here answers a question the core asked and nothing here is an approval: an action that needs a yes still asks for one. With no
// task running the answer is 409 and the words are a new request. Everything here is a plain function, so a test can call it without a page.

import { count, isObject, jsonInit, PlainError, failure, sendRequest, UNREADABLE_ANSWER } from './plainErrors.ts';

/** The longest steering line the core takes (core/src/steer.rs `MAX_CHARS`). */
export const STEER_MAX_CHARS = 500;

/** The words as the core reads them: on one line, whitespace squeezed (core/src/steer.rs `one_line`). */
export function steerLine(text: string): string {
  return text.split(/\s+/).filter((part) => part !== '').join(' ');
}

/** Why the words cannot be sent, in a sentence, or null when they can. */
export function steerProblem(text: string): string | null {
  const line = steerLine(text);
  if (line === '') return 'Say what to add.';
  if (Array.from(line).length > STEER_MAX_CHARS) return `A line is at most ${STEER_MAX_CHARS} characters.`;
  return null;
}

export type SteerResult = { kind: 'queued'; waiting: number | null; sentence: string } | { kind: 'stopped'; sentence: string };

export function queuedSentence(waiting: number | null): string {
  const base = 'Taken. Aqua reads it at its next step.';
  return waiting !== null && waiting > 1 ? `${base} ${waiting} lines are waiting.` : base;
}

export const STOPPED_SENTENCE = 'Stop sent. The task is ending.';
export const NOT_CONFIRMED = 'Aqua did not confirm that it took your words. Look at the task before you send them again.';

/** What the steer route answered, or null when it is not an answer that says the words were taken. */
export function mapSteer(status: number, body: unknown): SteerResult | null {
  if (!isObject(body)) return null;
  if (status >= 200 && status < 300 && body.stopped === true) return { kind: 'stopped', sentence: STOPPED_SENTENCE };
  if (status === 202 && typeof body.queued === 'number' && Number.isFinite(body.queued) && body.queued >= 0) {
    const waiting = count(body.queued);
    return { kind: 'queued', waiting, sentence: queuedSentence(waiting) };
  }
  return null;
}

/** Whether the core is serving a task now (GET /api/v1/tasks/status). */
export function mapBusy(body: unknown): boolean | null {
  return isObject(body) && typeof body.busy === 'boolean' ? body.busy : null;
}

export const steerApi = {
  /**
   * Sends the person's words to the task that runs. Throws a PlainError whose message is a sentence: the words are not sent when they are empty or too
   * long, and the box is told "taken" only when the core says so.
   */
  send: async (text: string): Promise<SteerResult> => {
    const problem = steerProblem(text);
    if (problem !== null) throw new PlainError(problem);
    const answer = await sendRequest('/api/v1/turn/steer', jsonInit('POST', { text: steerLine(text) }));
    if (!answer.ok) throw failure(answer, { fallback: 'Your words could not be sent.' });
    const result = mapSteer(answer.status, answer.body);
    if (!result) throw new PlainError(NOT_CONFIRMED, answer.status);
    return result;
  },

  /** Whether a task is running now, for any caller (API token is enough). */
  busy: async (): Promise<boolean> => {
    const answer = await sendRequest('/api/v1/tasks/status');
    if (!answer.ok) throw failure(answer, { fallback: 'Whether a task is running could not be read.' });
    const busy = mapBusy(answer.body);
    if (busy === null) throw new PlainError(UNREADABLE_ANSWER, answer.status);
    return busy;
  },
};
