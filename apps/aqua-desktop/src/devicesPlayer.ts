// Speaks the device announcements the core queues ("Rockerz 551ANC connected.", "Your headphones have 20 percent battery
// remaining.", "Warning. Your ... is critically low on battery.").
//
// The core decides what to say and when; this is only the player. It
//  * runs in ONE window: the one that holds the "aqua-device-speaker" lock (both windows start it, the lock picks one, and
//    when that window closes the other takes over),
//  * asks the core for the next item only when it may speak (never while the user is talking; only a critical item while a
//    task is thinking or speaking, so an answer is not cut and the model is not slowed for a connection notice),
//  * plays through its OWN audio element and never calls the answer player's stop function, so it neither cuts an answer
//    nor is cut by one; the two share a lock around each piece of audio ("aqua-audio-output"), so they never overlap,
//  * stops when the user stops speech (the orb's Stop, a spoken "stop", the task bar) and acknowledges every item, so the
//    core knows what was really spoken (a critical item that was not is spoken again).

import { meterAudioElement, subscribeActivity } from './activity';
import { devicesApi } from './api/devices';
import { openEventStream } from './api/stream';
import { synthesizeWav, withAudioLock } from './api/speech';
import { taskSnapshot } from './tasks';

const SPEAKER_LOCK = 'aqua-device-speaker';
export const STOP_CHANNEL = 'aqua-device-announce';

/** What the player is doing, for tests and debugging (like the orb's other `__orb*` hooks). */
const state = { running: false, events: 0, pending: 0, listening: false, drains: 0, last: 'not started', spoken: 0 };
(window as unknown as { __aquaDevicePlayer?: typeof state }).__aquaDevicePlayer = state;

let currentAudio: HTMLAudioElement | null = null;
let currentAbort: AbortController | null = null;

/** Stop whatever announcement is playing (the queue goes on with the next one, if it is allowed to). */
export function stopAnnouncement(): void {
  currentAbort?.abort();
  if (currentAudio) {
    currentAudio.pause();
    currentAudio = null;
  }
}

/** Ask every window to stop its announcement (used by the Stop buttons, which may be pressed in the other window). */
export function requestStopAnnouncements(): void {
  stopAnnouncement();
  try {
    const ch = new BroadcastChannel(STOP_CHANNEL);
    ch.postMessage('stop');
    ch.close();
  } catch {
    /* no BroadcastChannel: only this window is stopped */
  }
}

function playBlob(wav: string): Promise<void> {
  const bin = atob(wav);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const url = URL.createObjectURL(new Blob([bytes], { type: 'audio/wav' }));
  const audio = new Audio(url);
  meterAudioElement(audio, 'device-notice', 'Device notice'); // the orb follows it like any other speech
  currentAudio = audio;
  return new Promise((resolve, reject) => {
    const done = () => {
      URL.revokeObjectURL(url);
      if (currentAudio === audio) currentAudio = null;
    };
    audio.onended = () => {
      done();
      resolve();
    };
    audio.onpause = () => {
      // stopAnnouncement() pauses it: end quietly
      if (currentAudio !== audio) {
        done();
        resolve();
      }
    };
    audio.onerror = () => {
      done();
      reject(new Error('audio playback failed'));
    };
    audio.play().catch((e) => {
      done();
      reject(e);
    });
  });
}

/** Speak one announcement. True if it was spoken. */
async function speak(text: string): Promise<boolean> {
  const abort = new AbortController();
  currentAbort = abort;
  // The time limit covers making the speech (it may wait behind a running task), not playing it.
  const timer = window.setTimeout(() => abort.abort(), 60_000);
  try {
    const wav = await synthesizeWav(text, abort.signal);
    window.clearTimeout(timer);
    if (!wav || abort.signal.aborted) return false;
    await withAudioLock(() => (abort.signal.aborted ? Promise.resolve() : playBlob(wav)));
    return !abort.signal.aborted;
  } catch {
    return false;
  } finally {
    window.clearTimeout(timer);
    if (currentAbort === abort) currentAbort = null;
  }
}

/**
 * Start the player. Returns a function that stops it. Safe to call in every window: only the one that gets the lock plays.
 */
export function startDevicePlayer(): () => void {
  let stopped = false;
  let release: (() => void) | null = null;
  let cleanup: (() => void) | null = null;

  const run = () => {
    let listening = false;
    let pending = 0;
    let draining = false;

    state.running = true;
    const unsubActivity = subscribeActivity((m) => {
      listening = m.phase === 'listening';
      state.listening = listening;
    });

    const speakNext = async (): Promise<boolean> => {
      if (listening) {
        state.last = 'held: the user is talking';
        return false; // the user is talking: wait
      }
      const t = taskSnapshot().current;
      // While a task is thinking or speaking only a critical item may speak (an answer is not cut, the model is not slowed).
      const allow = t ? 'critical_only' : 'all';
      const res = await devicesApi.next(allow);
      const item = res.data as { id: number; text: string } | undefined;
      state.last = `next(${allow}) -> ${res.ok ? (item ? 'item ' + item.id : 'nothing') : res.error}`;
      if (!res.ok || !item || typeof item.id !== 'number') return false;
      const spoken = await speak(item.text);
      state.last = `item ${item.id} ${spoken ? 'spoken' : 'could not be spoken'}`;
      if (spoken) state.spoken += 1;
      await devicesApi.ack(item.id, spoken);
      return true;
    };

    const drain = async () => {
      if (draining || stopped) return;
      draining = true;
      state.drains += 1;
      try {
        for (let i = 0; i < 6 && !stopped; i++) {
          if (!(await speakNext())) break;
        }
      } finally {
        draining = false;
      }
    };

    // The stream is reopened whenever it ends or fails (the core may still be starting when this page loads, and the
    // connection is closed by every sleep and resume). While it is down the player still asks, slowly.
    let streamUp = false;
    let stream: { close: () => void } | null = null;
    let reconnect: number | undefined;
    let delay = 2000;
    const connect = () => {
      if (stopped) return;
      stream = openEventStream('/api/v1/devices/stream', {
        onEvent: (name, data) => {
          if (name !== 'snapshot') return;
          try {
            pending = (JSON.parse(data) as { pending?: number }).pending ?? 0;
            state.events += 1;
            state.pending = pending;
          } catch {
            return;
          }
          streamUp = true;
          delay = 2000;
          if (pending > 0) void drain();
        },
        onEnd: () => {
          streamUp = false;
          state.last = 'event stream ended; reconnecting';
          reconnect = window.setTimeout(connect, delay);
          delay = Math.min(delay * 2, 30_000);
        },
        onError: () => {
          streamUp = false;
          state.last = 'event stream error; reconnecting';
          reconnect = window.setTimeout(connect, delay);
          delay = Math.min(delay * 2, 30_000);
        },
      });
    };
    connect();
    // Held items are tried again now and then (a task ends, the user stops talking), and always while the stream is down.
    const retry = window.setInterval(() => {
      if (pending > 0 || !streamUp) void drain();
    }, 3000);
    // A stop pressed in the other window.
    let ch: BroadcastChannel | null = null;
    try {
      ch = new BroadcastChannel(STOP_CHANNEL);
      ch.onmessage = () => stopAnnouncement();
    } catch {
      ch = null;
    }
    cleanup = () => {
      stream?.close();
      if (reconnect !== undefined) window.clearTimeout(reconnect);
      window.clearInterval(retry);
      unsubActivity();
      ch?.close();
      stopAnnouncement();
    };
  };

  const locks = (navigator as Navigator & { locks?: LockManager }).locks;
  if (locks) {
    void locks.request(SPEAKER_LOCK, () => {
      if (stopped) return undefined;
      run();
      // hold the lock until this window stops the player or closes
      return new Promise<void>((resolve) => {
        release = resolve;
      });
    });
  } else {
    run();
  }

  return () => {
    stopped = true;
    cleanup?.();
    release?.();
  };
}
