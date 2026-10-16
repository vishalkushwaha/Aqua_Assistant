// Gives the announcements player (src/announcements.ts) its sound, its clock and its window: the sentences Aqua says on its own
// (a reminder that came due, a timer) are asked for from the core, shown as a toast (components/ReminderToast.tsx) and said with the
// app's own speech path, the local Kokoro engine, exactly as the device announcements are (src/devicesPlayer.ts).
//
//  * It runs in ONE window: the one that holds the "aqua-announcement-player" lock. The main window hides instead of closing, so
//    while Aqua runs the player runs. The voice orb's window does not start it: with the main window gone, reminders wait in the
//    core's queue (20 at most, none older than an hour) and are said when the main window comes back.
//  * It asks no faster than every 2 seconds (see `pollDelay`) and says nothing while the person is talking or a task is thinking or
//    speaking: it asks with `busy: true`, and the core hands out nothing then.
//  * Its sound goes through the same lock as every other speech of the app ("aqua-audio-output"), so two voices never overlap.
//  * Stop speaking (the task bar, the orb, a spoken "stop") stops it as it stops a device announcement: it listens on the same channel.

import { meterAudioElement, subscribeActivity } from './activity';
import { createPlayer, pollDelay, ToastQueue, type SpeakResult } from './announcements';
import { announcementsApi } from './api/announcements';
import { synthesizeWav, withAudioLock } from './api/speech';
import { STOP_CHANNEL } from './devicesPlayer';
import { taskSnapshot } from './tasks';

const PLAYER_LOCK = 'aqua-announcement-player';
/** The core holds a sentence for 45 seconds before it offers it again; making the speech is given less than that. */
const SYNTHESIS_LIMIT_MS = 30_000;

/** The toasts on screen and waiting: ReminderToast reads this. */
export const toasts = new ToastQueue();

interface Speech {
  abort: AbortController;
  audio: HTMLAudioElement | null;
  /** Stop was pressed while this sentence was made or played. */
  stopped: boolean;
}

let active: Speech | null = null;

/** Stops the sentence that is being said, if any. A Stop pressed when nothing is being said does nothing, now or later. */
function stopSpeech(): void {
  const speech = active;
  if (!speech) return;
  speech.stopped = true;
  speech.abort.abort();
  speech.audio?.pause();
}

function playWav(wav: string, speech: Speech): Promise<void> {
  const bin = atob(wav);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const url = URL.createObjectURL(new Blob([bytes], { type: 'audio/wav' }));
  const audio = new Audio(url);
  meterAudioElement(audio, 'reminder', 'Reminder'); // the orb follows it like any other speech
  speech.audio = audio;
  return new Promise((resolve, reject) => {
    const done = () => {
      URL.revokeObjectURL(url);
      speech.audio = null;
    };
    audio.onended = () => {
      done();
      resolve();
    };
    audio.onpause = () => {
      // stopSpeech() pauses it: end quietly
      if (speech.stopped) {
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

/** Says one sentence with the local speech engine. `failed` when the engine is not ready or the sound did not play. */
async function speak(sentence: string): Promise<SpeakResult> {
  const speech: Speech = { abort: new AbortController(), audio: null, stopped: false };
  active = speech;
  const limit = window.setTimeout(() => speech.abort.abort(), SYNTHESIS_LIMIT_MS);
  try {
    const wav = await synthesizeWav(sentence, speech.abort.signal);
    window.clearTimeout(limit);
    if (speech.stopped) return 'stopped';
    if (!wav) return 'failed';
    await withAudioLock(() => (speech.stopped ? Promise.resolve() : playWav(wav, speech)));
    return speech.stopped ? 'stopped' : 'spoken';
  } catch {
    return speech.stopped ? 'stopped' : 'failed';
  } finally {
    window.clearTimeout(limit);
    if (active === speech) active = null;
  }
}

/** Starts the player. Returns a function that stops it. Safe to call in every window: only the one that gets the lock plays. */
export function startAnnouncementPlayer(): () => void {
  let stopped = false;
  let release: (() => void) | null = null;
  let cleanup: (() => void) | null = null;

  const run = () => {
    let listening = false;
    let timer: number | undefined;
    const unsubscribe = subscribeActivity((m) => {
      listening = m.phase === 'listening';
    });
    const player = createPlayer({
      next: announcementsApi.next,
      ack: announcementsApi.ack,
      speak,
      isBusy: () => listening || taskSnapshot().current !== null,
      show: (item) => {
        toasts.push(item);
      },
    });

    // One round after another, never two at once: the next question is asked after the last sentence has been said and answered.
    const loop = async () => {
      if (stopped) return;
      try {
        await player.tick();
      } catch {
        /* a round does not throw; if it ever does, the player goes on with the next one */
      }
      if (stopped) return;
      timer = window.setTimeout(() => void loop(), pollDelay(player.failures(), document.hidden));
    };
    void loop();

    let channel: BroadcastChannel | null = null;
    try {
      channel = new BroadcastChannel(STOP_CHANNEL);
      channel.onmessage = () => stopSpeech();
    } catch {
      channel = null; // no BroadcastChannel: only a stop in this window's own code reaches it
    }
    cleanup = () => {
      if (timer !== undefined) window.clearTimeout(timer);
      unsubscribe();
      channel?.close();
      stopSpeech();
    };
  };

  const locks = (navigator as Navigator & { locks?: LockManager }).locks;
  if (locks) {
    void locks.request(PLAYER_LOCK, () => {
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
