import type { CoreStatusResponse } from '../api/coreStatus';

/** What every component that shows the core's state reads: the last status, and whether the first answer is still awaited. */
export interface CoreStatusSnapshot {
  status: CoreStatusResponse | null;
  loading: boolean;
}

/** How often the core is asked while it is up and answering. */
export const POLL_MS = 5000;
/** How often it is asked while it is not (still starting, or one failed answer being checked), so the page notices the
 * moment it is ready instead of up to 5 seconds later. */
export const RETRY_MS = 1000;
/** One failed answer (a busy core, a dropped connection) is not "offline": the last good status stays until this many in a row fail. */
export const FAILURES_BEFORE_OFFLINE = 2;

export interface Timers {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

const browserTimers: Timers = {
  setTimeout: (callback, ms) => globalThis.setTimeout(callback, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

function same(a: CoreStatusResponse | null, b: CoreStatusResponse | null): boolean {
  return a === b || (a !== null && b !== null && JSON.stringify(a) === JSON.stringify(b));
}

/**
 * One poller of the core's status for the whole page. Nine components used to start a poll of their own every 5 seconds (each
 * with its own cross-origin check first); now there is one request at a time however many are showing the state. It runs
 * only while something is subscribed, asks every second while the core is not up, and keeps the last good status through a
 * single failed answer. An unchanged status keeps the same object, so components do not redraw for nothing.
 */
export function createCoreStatusStore(
  fetchStatus: () => Promise<CoreStatusResponse | null>,
  connected: (status: CoreStatusResponse | null) => boolean,
  timers: Timers = browserTimers,
) {
  let snapshot: CoreStatusSnapshot = { status: null, loading: true };
  const listeners = new Set<() => void>();
  let timer: unknown;
  let running = false;
  let failures = 0;
  // Bumped whenever the loop starts or stops, so an answer that arrives after everyone left is dropped.
  let generation = 0;

  const publish = (status: CoreStatusResponse | null) => {
    const kept = same(status, snapshot.status) ? snapshot.status : status;
    if (kept === snapshot.status && !snapshot.loading) {
      return;
    }
    snapshot = { status: kept, loading: false };
    listeners.forEach((listener) => listener());
  };

  const poll = async () => {
    const mine = generation;
    let answer: CoreStatusResponse | null = null;
    try {
      answer = await fetchStatus();
    } catch {
      answer = null;
    }
    if (!running || mine !== generation) {
      return;
    }
    failures = answer ? 0 : failures + 1;
    const status = answer ?? (failures >= FAILURES_BEFORE_OFFLINE ? null : snapshot.status);
    publish(status);
    timer = timers.setTimeout(() => void poll(), failures === 0 && connected(status) ? POLL_MS : RETRY_MS);
  };

  const subscribe = (listener: () => void) => {
    listeners.add(listener);
    if (!running) {
      running = true;
      generation += 1;
      failures = 0;
      void poll();
    }
    return () => {
      listeners.delete(listener);
      if (listeners.size === 0) {
        running = false;
        generation += 1;
        if (timer !== undefined) {
          timers.clearTimeout(timer);
          timer = undefined;
        }
      }
    };
  };

  return { subscribe, getSnapshot: () => snapshot };
}
