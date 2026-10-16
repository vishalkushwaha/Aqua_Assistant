// The background runs, as the Queue panel shows them (GET /api/v1/queue) and cancelling one (POST /api/v1/queue/{job_id}/cancel).
// Both need the UI token, which the shell or the dev proxy adds: a goal and an answer are what the person asked for and got.
//
// A job is a request that runs in the background (not the one the person is talking to Aqua about). It waits its turn, runs one at a
// time, and pauses at a step while the person uses the model. Everything here is a plain function, so a test can call it without a page.
// Text from the core (a goal, an answer, an error) is only ever handed to React as a text child.

import { agoText, AquaError, bodyOrThrow, callCore, clip, count, isObject, isWholeNumber, jsonInit, positiveOrNull, spanText, text, UNREADABLE_ANSWER, type Wording } from './plain.ts';

export type JobState = 'queued' | 'running' | 'waiting' | 'done' | 'failed' | 'cancelled' | 'unknown';

export interface QueueJob {
  id: number;
  goal: string;
  state: JobState;
  /** How a finished run came out: `done`, `timed_out`, `needs_you`, `cancelled` or `failed`; '' while it has not ended. */
  outcome: string;
  queuedAtMs: number | null;
  startedAtMs: number | null;
  endedAtMs: number | null;
  /** How long the run waited for the person's own request, in all. */
  pausedMs: number;
  answer: string;
  error: string;
  /** While the state is `waiting`: what it waits for, in the core's words. */
  waitingFor: string;
}

export interface QueueView {
  /** As the core lists them: newest first. `orderJobs` puts the ones that matter now on top. */
  jobs: QueueJob[];
  /** How many more may wait before one is refused, or null when the core does not say. */
  maxWaiting: number | null;
  /** How many turns of the person's own are being answered now. */
  foregroundActive: number;
  /** The run that is going is paused because the person is using the model. */
  waitingForThePerson: boolean;
}

const STATES: readonly JobState[] = ['queued', 'running', 'waiting', 'done', 'failed', 'cancelled'];

/** One job, or null when it has no whole-number id (nothing could be cancelled by it). Missing fields become plain defaults. */
export function mapJob(raw: unknown): QueueJob | null {
  if (!isObject(raw) || !isWholeNumber(raw.id)) return null;
  const state = STATES.find((s) => s === raw.state) ?? 'unknown';
  return {
    id: raw.id,
    goal: text(raw.goal),
    state,
    outcome: text(raw.outcome),
    queuedAtMs: positiveOrNull(raw.queued_at_ms),
    startedAtMs: positiveOrNull(raw.started_at_ms),
    endedAtMs: positiveOrNull(raw.ended_at_ms),
    pausedMs: count(raw.paused_ms),
    answer: text(raw.answer),
    error: text(raw.error),
    waitingFor: text(raw.waiting_for),
  };
}

/** The answer of the list route as the panel uses it, or null when it is not a list of jobs at all. */
export function mapQueue(body: unknown): QueueView | null {
  if (!isObject(body) || !Array.isArray(body.jobs)) return null;
  const jobs: QueueJob[] = [];
  for (const row of body.jobs) {
    const job = mapJob(row);
    if (job) jobs.push(job);
  }
  const depth = isObject(body.depth) ? body.depth : {};
  const max = body.max_waiting ?? depth.max_waiting;
  return {
    jobs,
    maxWaiting: isWholeNumber(max) ? max : null,
    foregroundActive: count(body.foreground_active),
    waitingForThePerson: depth.waiting_for_the_person === true,
  };
}

export function isActive(job: QueueJob): boolean {
  return job.state === 'queued' || job.state === 'running' || job.state === 'waiting';
}

/** A job can be cancelled while it has not ended. (A paused run is a running one that waits for the person: it can be told to stop.) */
export function canCancel(job: QueueJob): boolean {
  return isActive(job);
}

/**
 * The runs that matter now first: the one that is going, then the ones waiting with the next to run on top (the oldest), then the
 * ended ones, newest first. The array given is not reordered in place.
 */
export function orderJobs(jobs: QueueJob[]): QueueJob[] {
  const rank = (j: QueueJob) => (j.state === 'running' || j.state === 'waiting' ? 0 : j.state === 'queued' ? 1 : 2);
  const time = (j: QueueJob) => j.queuedAtMs ?? 0;
  return [...jobs].sort((a, b) => {
    const r = rank(a) - rank(b);
    if (r !== 0) return r;
    if (rank(a) === 1) return time(a) - time(b) || a.id - b.id;
    return time(b) - time(a) || b.id - a.id;
  });
}

export function stateText(job: QueueJob): string {
  switch (job.state) {
    case 'queued': return 'Waiting for its turn';
    case 'running': return 'Running';
    case 'waiting': return 'Paused while you use Aqua';
    case 'done': return job.outcome === 'needs_you' ? 'Stopped to ask you something' : 'Finished';
    case 'failed': return job.outcome === 'timed_out' ? 'Ran out of time' : 'Failed';
    case 'cancelled': return 'Cancelled';
    default: return 'State unknown';
  }
}

export type JobTone = 'ok' | 'attention' | 'problem' | 'neutral';

export function stateTone(job: QueueJob): JobTone {
  switch (job.state) {
    case 'running': return 'ok';
    case 'waiting': return 'attention';
    case 'done': return job.outcome === 'needs_you' ? 'attention' : 'ok';
    case 'failed': return 'problem';
    default: return 'neutral';
  }
}

/** The line under a goal: how long it has waited or run, or when it ended and how long it took. `nowMs` is given so the answer does not depend on the clock. */
export function timingText(job: QueueJob, nowMs: number): string {
  const since = (ms: number | null) => (ms === null ? null : spanText((nowMs - ms) / 1000));
  switch (job.state) {
    case 'queued': {
      const waited = since(job.queuedAtMs);
      return waited === null ? '' : `waiting for ${waited}`;
    }
    case 'running':
    case 'waiting': {
      const ran = since(job.startedAtMs);
      return ran === null ? '' : `started ${ran} ago`;
    }
    default: {
      const parts: string[] = [];
      if (job.endedAtMs !== null) parts.push(`ended ${agoText(job.endedAtMs / 1000, nowMs / 1000)}`);
      if (job.startedAtMs !== null && job.endedAtMs !== null && job.endedAtMs >= job.startedAtMs) parts.push(`took ${spanText((job.endedAtMs - job.startedAtMs) / 1000)}`);
      return parts.join(' · ');
    }
  }
}

/** One sentence for the top of the panel. */
export function summaryText(view: QueueView): string {
  const running = view.jobs.filter((j) => j.state === 'running' || j.state === 'waiting').length;
  const queued = view.jobs.filter((j) => j.state === 'queued').length;
  if (running === 0 && queued === 0) return view.foregroundActive > 0 ? 'Nothing runs in the background. Aqua is answering you now.' : 'Nothing runs in the background and nothing waits.';
  const parts: string[] = [];
  if (running > 0) parts.push(running === 1 ? '1 is running' : `${running} are running`);
  if (queued > 0) parts.push(queued === 1 ? '1 is waiting' : `${queued} are waiting`);
  let line = `Background runs: ${parts.join(', ')}.`;
  if (view.maxWaiting !== null) line += ` At most ${view.maxWaiting} may wait.`;
  if (view.waitingForThePerson || view.foregroundActive > 0) line += ' A run goes on at its next step once your own request is answered.';
  return line;
}

/** The goal as listed: cut when very long, and a stand-in when the core sent none. */
export function goalText(goal: string): string {
  const shown = goal.trim();
  return shown === '' ? '(no goal recorded)' : clip(shown, 160);
}

/** What a finished run said (cut), or ''. */
export function answerText(job: QueueJob): string {
  return clip(job.answer.trim(), 300);
}

/** The question asked before a cancel is sent. */
export function cancelQuestion(job: QueueJob): string {
  return job.state === 'queued' ? 'Take this run out of the queue?' : 'Tell this run to stop? It ends at its next step.';
}

/** The sentence under a cancel that the core accepted. `state` is what the core answered (`cancelled` for a waiting run, `running` for one that was told to stop). */
export function cancelledText(state: string): string {
  if (state === 'cancelled') return 'Took that run out of the queue.';
  if (state === 'running') return 'Told that run to stop. It ends at its next step.';
  return 'Asked Aqua to cancel that run.';
}

const LIST_WORDING: Wording = { fallback: 'The background runs could not be read.', status: { 404: 'This version of Aqua cannot list background runs.' } };
const CANCEL_WORDING: Wording = { fallback: 'That run could not be cancelled.', status: { 404: 'This version of Aqua cannot cancel a background run.', 409: 'That run has ended already, or it is not there any more.' } };

export const queueApi = {
  /** The background runs and whether the person is being served now (UI token). */
  list: async (): Promise<QueueView> => {
    const view = mapQueue(bodyOrThrow(await callCore('/api/v1/queue', undefined, LIST_WORDING)));
    if (!view) throw new AquaError(UNREADABLE_ANSWER);
    return view;
  },
  /** Takes a waiting run out, or tells a running one to stop at its next step (UI token). `state` is the core's word for what became of it. */
  cancel: async (id: number): Promise<{ state: string }> => {
    if (!isWholeNumber(id)) throw new AquaError(CANCEL_WORDING.fallback);
    const body = bodyOrThrow(await callCore(`/api/v1/queue/${id}/cancel`, jsonInit('POST'), CANCEL_WORDING));
    return { state: isObject(body) ? text(body.state) : '' };
  },
};
