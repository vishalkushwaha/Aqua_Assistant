// One task at a time, across the admin panel and the voice orb.
//
// A task is anything the assistant is asked to do: a voice request, a typed one, an agent run started in the admin panel.
// It covers the whole job, thinking and then speaking the answer, so the next task never starts while the last one is
// still talking. Tasks wait in the order they were given; a task starts only after the one before it has finished or been
// stopped and everything it was using has been let go.
//
// The line is kept with the browser's cross-window lock (first come, first served, exclusive, and released by itself if
// a window dies), so it holds even though the orb and the admin panel are separate windows. Windows tell each other what
// is running and waiting over a BroadcastChannel, which is what the orb's menu and the admin panel's task bar read.
// Stop is sent the same way, to whichever window owns the running task, and also straight to the core, which hangs up
// its model call and ends its own work before answering.

import { useEffect, useState } from 'react';

export type TaskPhase = 'queued' | 'thinking' | 'speaking';

export interface TaskInfo {
  id: string;
  label: string;
  phase: TaskPhase;
  owner: string; // the window that runs it
  queuedAt: number;
}

export interface TaskSnapshot {
  current: TaskInfo | null; // the one running now
  queued: TaskInfo[]; // waiting, in order
}

export interface TaskContext {
  id: string;
  /** Aborts when the task is stopped: pass it to fetch() and stop any sound. */
  signal: AbortSignal;
  /** Tell everyone what the task is doing now (thinking, then speaking). */
  setPhase: (phase: 'thinking' | 'speaking') => void;
}

export interface TaskResult<T> {
  stopped: boolean; // the person stopped it (or it was stopped while waiting)
  value?: T;
  error?: unknown;
}

const LOCK = 'aqua-task-queue';
const CHANNEL = 'aqua-tasks';
const HEARTBEAT_MS = 2000;
const STALE_MS = 6000; // a window that stops reporting is treated as gone

type Wire =
  | { type: 'tasks'; owner: string; tasks: TaskInfo[] }
  | { type: 'stop' };

const owner = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
const mine = new Map<string, TaskInfo>();
const controllers = new Map<string, AbortController>();
const others = new Map<string, { tasks: TaskInfo[]; seen: number }>();
const listeners = new Set<(s: TaskSnapshot) => void>();
let runningId: string | null = null;
let bus: BroadcastChannel | null = null;

function ensureBus(): void {
  if (bus) return;
  try {
    bus = new BroadcastChannel(CHANNEL);
  } catch {
    return; // no BroadcastChannel: this window just works alone
  }
  bus.onmessage = (e: MessageEvent<Wire>) => {
    const m = e.data;
    if (m?.type === 'tasks' && m.owner !== owner) {
      others.set(m.owner, { tasks: m.tasks, seen: Date.now() });
      emit();
    } else if (m?.type === 'stop') {
      abortRunning();
    }
  };
  window.setInterval(() => {
    const now = Date.now();
    let changed = false;
    for (const [k, v] of others) if (now - v.seen > STALE_MS) { others.delete(k); changed = true; }
    if (mine.size > 0) announce(); // so other windows keep this window's tasks on their list
    if (changed) emit();
  }, HEARTBEAT_MS);
}

function snapshot(): TaskSnapshot {
  const all: TaskInfo[] = [...mine.values()];
  for (const v of others.values()) all.push(...v.tasks);
  const current = all.filter((t) => t.phase !== 'queued').sort((a, b) => a.queuedAt - b.queuedAt)[0] ?? null;
  const queued = all.filter((t) => t.phase === 'queued').sort((a, b) => a.queuedAt - b.queuedAt);
  return { current, queued };
}

function emit(): void {
  const s = snapshot();
  for (const l of listeners) l(s);
}

function announce(): void {
  bus?.postMessage({ type: 'tasks', owner, tasks: [...mine.values()] } satisfies Wire);
}

function changed(): void {
  announce();
  emit();
}

function abortRunning(): void {
  if (runningId) controllers.get(runningId)?.abort();
}

/** Tell the core to stop what it is doing, and wait until it says it is idle. Never throws. */
export async function stopCore(): Promise<void> {
  try {
    await fetch('/api/v1/tasks/stop', { method: 'POST' });
  } catch {
    /* the core may be gone or restarting: there is nothing left to stop */
  }
}

/**
 * Stop the running task now, wherever it runs. Waiting tasks are left alone and the next one starts once this one has
 * let go of everything. Safe to call when nothing is running.
 */
export function stopTask(): void {
  stopAnnouncementsEverywhere();
  ensureBus();
  bus?.postMessage({ type: 'stop' } satisfies Wire);
  abortRunning(); // BroadcastChannel does not echo to the window that sent it
  void stopCore(); // also straight to the core, in case the window that runs the task is gone
}

/** A device announcement that is being spoken is stopped too (Stop speaking applies to it as well). Not part of the task queue. */
function stopAnnouncementsEverywhere(): void {
  try {
    const ch = new BroadcastChannel('aqua-device-announce');
    ch.postMessage('stop');
    ch.close();
  } catch {
    /* no BroadcastChannel: nothing to stop in other windows */
  }
}

/** Let go of a task that is still waiting for its turn. */
export function cancelQueuedTask(id: string): void {
  controllers.get(id)?.abort();
}

function newId(): string {
  return `${owner}-${Math.random().toString(36).slice(2, 9)}`;
}

/**
 * Run `body` as a task: it waits its turn, runs alone, and the next task starts only after it ends. `body` should resolve
 * when the whole job is done, including any speech, and should end quickly when `ctx.signal` aborts.
 * Resolves with `stopped: true` if the task was stopped, with the body's error if it failed.
 */
export async function runTask<T>(
  label: string,
  body: (ctx: TaskContext) => Promise<T>,
  opts: { maxMs?: number; onQueued?: (ahead: number) => void; onId?: (id: string) => void } = {},
): Promise<TaskResult<T>> {
  ensureBus();
  const id = newId();
  opts.onId?.(id); // so the caller can cancel it while it is still waiting
  const controller = new AbortController();
  controllers.set(id, controller);
  mine.set(id, { id, label, phase: 'queued', owner, queuedAt: Date.now() });
  changed();
  // Tasks ahead of this one: the one running, and any queued before it
  const line = snapshot();
  const ahead = (line.current ? 1 : 0) + Math.max(0, line.queued.findIndex((t) => t.id === id));
  if (ahead > 0) opts.onQueued?.(ahead);

  const finish = () => {
    controllers.delete(id);
    mine.delete(id);
    if (runningId === id) runningId = null;
    changed();
  };

  const runAlone = async (): Promise<TaskResult<T>> => {
    if (controller.signal.aborted) return { stopped: true };
    runningId = id;
    mine.set(id, { ...mine.get(id)!, phase: 'thinking' });
    changed();
    const timer = opts.maxMs ? window.setTimeout(() => controller.abort(), opts.maxMs) : 0;
    const ctx: TaskContext = {
      id,
      signal: controller.signal,
      setPhase: (phase) => {
        const t = mine.get(id);
        if (t && t.phase !== phase) { mine.set(id, { ...t, phase }); changed(); }
      },
    };
    let result: TaskResult<T>;
    try {
      result = { stopped: false, value: await body(ctx) };
    } catch (error) {
      result = controller.signal.aborted ? { stopped: true } : { stopped: false, error };
    } finally {
      if (timer) window.clearTimeout(timer);
    }
    if (controller.signal.aborted) {
      result = { ...result, stopped: true };
      // Everything the task was doing must have ended before the next one starts, so the core is asked to stop and
      // confirms it is idle (the task's own request was already hung up).
      await stopCore();
    }
    return result;
  };

  try {
    if (navigator.locks) {
      return await navigator.locks.request(LOCK, { mode: 'exclusive', signal: controller.signal }, runAlone);
    }
    return await runAlone(); // no cross-window lock in this webview: tasks in this window still run one by one below
  } catch (error) {
    // Aborted while still waiting for its turn
    if (controller.signal.aborted) return { stopped: true };
    return { stopped: false, error };
  } finally {
    finish();
  }
}

/** The current task and the line behind it, live. */
export function useTasks(): TaskSnapshot {
  const [state, setState] = useState<TaskSnapshot>(() => { ensureBus(); return snapshot(); });
  useEffect(() => {
    ensureBus();
    listeners.add(setState);
    setState(snapshot());
    return () => { listeners.delete(setState); };
  }, []);
  return state;
}

/** For code outside React: what is running now. */
export function currentTask(): TaskInfo | null {
  ensureBus();
  return snapshot().current;
}

/** For code outside React: the current task and the line behind it. */
export function taskSnapshot(): TaskSnapshot {
  ensureBus();
  return snapshot();
}
