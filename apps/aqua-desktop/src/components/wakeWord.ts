import { requireJson } from '../api/jsonResponse.ts';

// Always-on wake word: while the orb is idle, the microphone stays open and each stretch of speech is checked by the core
// (/voice/wake-check: a small local Whisper model, nothing leaves this machine). When the assistant's name ("Hey Aqua") or
// "wake up" is said anywhere in a sentence, onWake fires. Audio that is not speech, and speech without the phrase, is
// thrown away. The check is cheap on purpose: the core uses a small model, a lane of its own that never makes a real request
// wait, and skips a clip (it says why) instead of queueing it when something else is running.

const TARGET_RATE = 16000;
const START_LEVEL = 0.02; // RMS that begins an utterance
const KEEP_LEVEL = 0.012; // RMS that keeps one going
const END_SILENCE_MS = 700; // quiet this long ends the utterance
const MIN_SPEECH_MS = 350; // shorter than this is a click or a cough
const MAX_UTTERANCE_MS = 10000;
// The wake phrase comes at the start of what is said, so only the first seconds of a stretch of speech are sent to be checked; the
// rest of a long stretch (a TV, a conversation) is not sent at all until it pauses. Without this a continuous voice made one
// check, one speech-recognition process, every 10 seconds, for as long as it went on (about 1,200 a day in the log).
const WAKE_MAX_UTTERANCE_MS = 4000;
const PRE_ROLL_MS = 400; // audio kept from before the speech started, so the first word is not cut
const ERROR_BACKOFF_MS = 5000;
const SKIPPED_COOLDOWN_MS = 1500; // the core was busy: leave it alone for a moment

/** What the listener waits for: the wake phrase while idle, or a spoken "stop" while a task is running. */
export type ListenMode = 'wake' | 'stop';

interface WakeCheck {
  wake: boolean;
  heard: string;
  /** Why the core did not check the clip: busy, too_short, too_quiet or too_slow. */
  skipped?: string;
}

function pcmToWavBase64(pcm: Float32Array, srcRate: number): string {
  const ratio = srcRate / TARGET_RATE;
  const n = Math.floor(pcm.length / ratio);
  const buf = new ArrayBuffer(44 + n * 2);
  const v = new DataView(buf);
  const str = (o: number, s: string) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
  str(0, 'RIFF'); v.setUint32(4, 36 + n * 2, true); str(8, 'WAVE'); str(12, 'fmt ');
  v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
  v.setUint32(24, TARGET_RATE, true); v.setUint32(28, TARGET_RATE * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true);
  str(36, 'data'); v.setUint32(40, n * 2, true);
  for (let i = 0; i < n; i++) {
    const pos = i * ratio;
    const i0 = Math.floor(pos);
    const frac = pos - i0;
    const s = Math.max(-1, Math.min(1, pcm[i0] * (1 - frac) + (pcm[Math.min(i0 + 1, pcm.length - 1)] ?? 0) * frac));
    v.setInt16(44 + i * 2, s < 0 ? s * 0x8000 : s * 0x7fff, true);
  }
  const bytes = new Uint8Array(buf);
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(bin);
}

async function checkWake(pcm: Float32Array, srcRate: number, mode: ListenMode): Promise<WakeCheck> {
  // The stop lane in the core works while a task is running (the wake lane stands aside for it)
  const res = await fetch(mode === 'stop' ? '/api/v1/voice/stop-check' : '/api/v1/voice/wake-check', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ wav_base64: pcmToWavBase64(pcm, srcRate) }),
  });
  const data = await requireJson<Record<string, unknown>>(res, 'Wake-word checking is unavailable.');
  return { wake: (mode === 'stop' ? data.stop : data.wake) === true, heard: String(data.heard ?? ''), skipped: typeof data.skipped === 'string' ? data.skipped : undefined };
}

export interface WakeListener {
  stop: () => void;
}

/**
 * Open the microphone and listen until stop() is called (which also releases the microphone). `onWake` is called when the
 * phrase for the mode is heard: the wake phrase (default) or an order to stop.
 */
export function startWakeListener(opts: { mode?: ListenMode; onWake: (heard: string) => void; onError?: (e: unknown) => void }): WakeListener {
  const mode: ListenMode = opts.mode ?? 'wake';
  const minSpeechMs = mode === 'stop' ? 250 : MIN_SPEECH_MS; // "stop" is a short word
  const cooldownMs = mode === 'stop' ? 600 : SKIPPED_COOLDOWN_MS; // an order to stop should not wait long for the next try
  const endSilenceMs = mode === 'stop' ? 450 : END_SILENCE_MS; // and is sent for checking sooner after the last word
  const maxUtteranceMs = mode === 'stop' ? MAX_UTTERANCE_MS : WAKE_MAX_UTTERANCE_MS;
  let stopped = false;
  let cleanup: (() => void) | null = null;

  void (async () => {
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
    } catch (e) {
      opts.onError?.(e);
      return;
    }
    if (stopped) { stream.getTracks().forEach((t) => t.stop()); return; }

    const ctx = new AudioContext();
    const source = ctx.createMediaStreamSource(stream);
    const processor = ctx.createScriptProcessor(4096, 1, 1);
    const mute = ctx.createGain();
    mute.gain.value = 0; // the processor only runs while connected to an output; this keeps it silent
    source.connect(processor);
    processor.connect(mute);
    mute.connect(ctx.destination);

    const chunkMs = (4096 / ctx.sampleRate) * 1000;
    const preRollChunks = Math.ceil(PRE_ROLL_MS / chunkMs);
    let preRoll: Float32Array[] = [];
    let utterance: Float32Array[] = [];
    let speaking = false;
    let voiceMs = 0;
    let quietMs = 0;
    let busy = false; // one transcription at a time; speech heard meanwhile is dropped
    let skipRest = false; // the rest of a long stretch of speech that was cut off at the limit: not checked until it pauses
    let pausedUntil = 0;

    const finish = async () => {
      const chunks = utterance;
      const spoke = voiceMs;
      utterance = []; speaking = false; voiceMs = 0; quietMs = 0;
      if (spoke < minSpeechMs || busy || Date.now() < pausedUntil) return;
      busy = true;
      try {
        const total = chunks.reduce((n, c) => n + c.length, 0);
        const pcm = new Float32Array(total);
        let off = 0;
        for (const c of chunks) { pcm.set(c, off); off += c.length; }
        const r = await checkWake(pcm, ctx.sampleRate, mode);
        // Busy or slow: the core is doing something that matters more; stay quiet for a moment (no error, no retry).
        if (r.skipped === 'busy' || r.skipped === 'too_slow') pausedUntil = Date.now() + cooldownMs;
        if (!stopped && r.wake) opts.onWake(r.heard);
      } catch (e) {
        opts.onError?.(e);
        pausedUntil = Date.now() + ERROR_BACKOFF_MS; // the core may be starting or busy; do not hammer it
      } finally {
        busy = false;
      }
    };

    processor.onaudioprocess = (ev) => {
      if (stopped) return;
      const data = ev.inputBuffer.getChannelData(0);
      const chunk = new Float32Array(data); // the engine reuses its buffer
      let sum = 0;
      for (let i = 0; i < chunk.length; i++) sum += chunk[i] * chunk[i];
      const rms = Math.sqrt(sum / chunk.length);

      if (!speaking) {
        if (skipRest) {
          if (rms > KEEP_LEVEL) quietMs = 0;
          else {
            quietMs += chunkMs;
            if (quietMs >= endSilenceMs) { skipRest = false; quietMs = 0; }
          }
          return;
        }
        preRoll.push(chunk);
        if (preRoll.length > preRollChunks) preRoll.shift();
        if (rms > START_LEVEL) {
          speaking = true;
          utterance = [...preRoll];
          preRoll = [];
          voiceMs = chunkMs;
          quietMs = 0;
        }
        return;
      }
      utterance.push(chunk);
      if (rms > KEEP_LEVEL) { voiceMs += chunkMs; quietMs = 0; } else { quietMs += chunkMs; }
      if (quietMs >= endSilenceMs) void finish();
      else if (utterance.length * chunkMs >= maxUtteranceMs) {
        skipRest = mode === 'wake';
        void finish();
      }
    };

    cleanup = () => {
      processor.onaudioprocess = null;
      try { source.disconnect(); processor.disconnect(); mute.disconnect(); } catch { /* already disconnected */ }
      stream.getTracks().forEach((t) => t.stop());
      void ctx.close();
    };
    if (stopped) cleanup();
  })();

  return {
    stop: () => {
      stopped = true;
      cleanup?.();
      cleanup = null;
    },
  };
}
