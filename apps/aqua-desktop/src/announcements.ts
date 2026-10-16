// The player of the sentences Aqua says on its own (a reminder that came due, a timer), without a page or a sound card in it, so that
// `node --test` can drive it step by step. src/announcementsPlayer.ts gives it the real speech, clock and window; ReminderToast.tsx shows
// what it queues.
//
// The rules (documentation/UI_CONTRACT.md section 4, core/src/flows/notify_voice.rs):
//  * the player takes one sentence with `next`, shows it, says it, and answers `ack` with `spoken`: true takes it off the core's queue,
//    false offers it again (three tries in all). There is no other answer.
//  * a sentence is shown once and said once. The core may hand the same id out again (the answer was lost, or the lease of 45 seconds
//    ran out): the player remembers what it did and only answers, it does not speak twice.
//  * a Stop pressed by the person is an answer: the sentence is let go (`spoken: true`), not offered again to be said once more.
//  * when an answer could not be sent, the player sends it before it asks for anything else.
//  * `busy` true (an answer is being spoken, the person is talking) hands out nothing: the sentence waits in the core's queue.

import type { AckAnswer, Announcement, NextAnswer } from './api/announcements.ts';

/** Never faster than every 2 seconds while the app is in view; slower while its window is hidden (it hides instead of closing). */
export const POLL_MS = 2000;
export const HIDDEN_POLL_MS = 4000;
export const MAX_BACKOFF_MS = 30_000;

/** The wait before the next question: the pace, doubled for each failure in a row (the core is down or starting), up to 30 seconds. */
export function pollDelay(failuresInARow: number, hidden: boolean): number {
  const base = hidden ? HIDDEN_POLL_MS : POLL_MS;
  const doublings = Number.isFinite(failuresInARow) ? Math.min(Math.max(0, Math.floor(failuresInARow)), 6) : 0;
  return Math.min(MAX_BACKOFF_MS, base * 2 ** doublings);
}

/** What the toast says the sentence is for. */
export function kindLabel(kind: string, sentence: string): string {
  switch (kind) {
    case 'reminder': return /^your timer is done\b/i.test(sentence.trim()) ? 'Timer' : 'Reminder';
    case 'succeeded': return 'A flow finished';
    case 'failed': return 'A flow stopped';
    case 'stalled': return 'A flow is quiet';
    case 'campaign': return 'A campaign finished';
    default: return 'Aqua says';
  }
}

// ---- the toast queue --------------------------------------------------------------------------------------------------------------

export interface ToastState {
  /** The one on screen, or null. */
  current: Announcement | null;
  /** How many more wait behind it. */
  waiting: number;
}

/** At most this many wait behind the one on screen (the core keeps 20 in all): the oldest waiting one is let go for a new one. */
export const MAX_WAITING_TOASTS = 20;
const MAX_REMEMBERED = 200;

/**
 * One toast at a time; the others wait their turn and show when the one on screen is dismissed. A sentence that was queued once is
 * never queued again, even when the core hands it out a second time. Made for `useSyncExternalStore`: `subscribe` and `getSnapshot`
 * can be passed as they are, and the snapshot is the same object until something changes.
 */
export class ToastQueue {
  private items: Announcement[] = [];
  private remembered = new Set<number>();
  private listeners = new Set<() => void>();
  private state: ToastState = { current: null, waiting: 0 };

  /** Queues a sentence. False when this id was queued before (it is not shown twice). */
  push(item: Announcement): boolean {
    if (this.remembered.has(item.id)) return false;
    this.remembered.add(item.id);
    if (this.remembered.size > MAX_REMEMBERED) {
      const oldest = this.remembered.values().next().value;
      if (oldest !== undefined) this.remembered.delete(oldest);
    }
    this.items.push(item);
    // The one on screen stays; the oldest of the others goes.
    while (this.items.length > 1 + MAX_WAITING_TOASTS) this.items.splice(1, 1);
    this.publish();
    return true;
  }

  /** Dismisses the one on screen; the next one, if any, comes up. */
  dismiss(): void {
    if (this.items.length === 0) return;
    this.items.shift();
    this.publish();
  }

  /** Forgets everything on screen and waiting (not what was remembered as shown). */
  clear(): void {
    if (this.items.length === 0) return;
    this.items = [];
    this.publish();
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): ToastState => this.state;

  private publish(): void {
    this.state = { current: this.items[0] ?? null, waiting: Math.max(0, this.items.length - 1) };
    for (const listener of [...this.listeners]) listener();
  }
}

// ---- the player -------------------------------------------------------------------------------------------------------------------

/** How saying a sentence ended. `stopped`: the person pressed Stop (or the window went away) while it was made or played. */
export type SpeakResult = 'spoken' | 'stopped' | 'failed';

export interface PlayerDeps {
  next(busy: boolean): Promise<NextAnswer>;
  ack(id: number, spoken: boolean): Promise<AckAnswer>;
  speak(sentence: string): Promise<SpeakResult>;
  /** An answer is being spoken, a task is thinking or the person is talking: nothing is handed out now. */
  isBusy(): boolean;
  /** Shows the toast. Called once for each sentence, however often the core hands it out. */
  show(item: Announcement): void;
}

export type TickOutcome =
  | { type: 'skipped' }
  | { type: 'idle' }
  | { type: 'busy' }
  | { type: 'spoken'; id: number }
  | { type: 'stopped'; id: number }
  | { type: 'not_spoken'; id: number }
  | { type: 'already_said'; id: number }
  | { type: 'unusable'; id: number }
  | { type: 'answer_owed'; id: number }
  | { type: 'core_error'; sentence: string };

const MAX_REMEMBERED_IDS = 200;

function remember(set: Set<number>, id: number): void {
  set.add(id);
  if (set.size > MAX_REMEMBERED_IDS) {
    const oldest = set.values().next().value;
    if (oldest !== undefined) set.delete(oldest);
  }
}

export interface Player {
  /** One round: send an answer that is owed, ask for the next sentence, show it, say it, answer. Never throws; never overlaps itself. */
  tick(): Promise<TickOutcome>;
  /** How many failures in a row the core had (for `pollDelay`). */
  failures(): number;
}

export function createPlayer(deps: PlayerDeps): Player {
  let running = false;
  let failures = 0;
  // An answer the core did not get (the call failed): sent first, before anything is asked.
  let owed: { id: number; spoken: boolean } | null = null;
  const shown = new Set<number>();
  const said = new Set<number>();

  const answer = async (id: number, spoken: boolean): Promise<boolean> => {
    let result: AckAnswer;
    try {
      result = await deps.ack(id, spoken);
    } catch {
      result = { type: 'failed', sentence: '', status: null };
    }
    if (result.type === 'failed') {
      owed = { id, spoken };
      return false;
    }
    owed = null;
    return true;
  };

  const round = async (): Promise<TickOutcome> => {
    if (owed) {
      const { id, spoken } = owed;
      if (!(await answer(id, spoken))) {
        failures += 1;
        return { type: 'answer_owed', id };
      }
    }
    const busy = deps.isBusy();
    let got: NextAnswer;
    try {
      got = await deps.next(busy);
    } catch {
      got = { type: 'failed', sentence: 'The next announcement could not be asked for.', status: null };
    }
    if (got.type === 'failed') {
      failures += 1;
      return { type: 'core_error', sentence: got.sentence };
    }
    failures = 0;
    if (got.type === 'none') return { type: busy ? 'busy' : 'idle' };
    if (got.type === 'unusable') {
      await answer(got.id, false);
      return { type: 'unusable', id: got.id };
    }
    const item = got.item;
    if (said.has(item.id)) {
      // Handed out again after it was said (the answer was lost): only the answer is sent again.
      await answer(item.id, true);
      return { type: 'already_said', id: item.id };
    }
    if (!shown.has(item.id)) {
      remember(shown, item.id);
      deps.show(item);
    }
    let result: SpeakResult;
    try {
      result = await deps.speak(item.text);
    } catch {
      result = 'failed';
    }
    if (result === 'failed') {
      await answer(item.id, false);
      return { type: 'not_spoken', id: item.id };
    }
    remember(said, item.id);
    await answer(item.id, true);
    return { type: result === 'stopped' ? 'stopped' : 'spoken', id: item.id };
  };

  return {
    tick: async () => {
      if (running) return { type: 'skipped' };
      running = true;
      try {
        return await round();
      } finally {
        running = false;
      }
    },
    failures: () => failures,
  };
}
