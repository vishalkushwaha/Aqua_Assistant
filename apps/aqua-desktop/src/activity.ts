// What the assistant is doing, shared between the admin panel and the voice orb.
//
// The two are separate windows of the same app (same origin), so they talk over a BroadcastChannel: a page in the
// admin panel says "I am listening / working / speaking" and the orb shows the same, in real time. Nothing leaves this
// computer. The orb ignores all of it while it is busy with something of its own.

import { useEffect, useRef } from 'react';

export type ActivityPhase = 'idle' | 'listening' | 'thinking' | 'speaking';

export interface ActivityMessage {
  source: string; // an id for what is going on, e.g. "whisper-test"; one page can have several (its task, its speech)
  label?: string; // the name shown in the orb, e.g. "Whisper test" (defaults to the id)
  phase: ActivityPhase;
  level?: number; // voice level 0..1 while listening or speaking
  ts: number;
}

const CHANNEL = 'aqua-activity';
const HEARTBEAT_MS = 2000; // repeated while something is going on, so a page that disappears does not leave the orb stuck
const LEVEL_EVERY_MS = 50;

let sender: BroadcastChannel | null = null;
function channel(): BroadcastChannel | null {
  if (sender) return sender;
  try {
    sender = new BroadcastChannel(CHANNEL);
  } catch {
    sender = null; // no BroadcastChannel: the orb simply does not mirror
  }
  return sender;
}

export function publishActivity(source: string, phase: ActivityPhase, level?: number, label?: string): void {
  channel()?.postMessage({ source, phase, level, label, ts: Date.now() } satisfies ActivityMessage);
}

/** The orb's side: calls back with every message from the admin panel. Returns a function that stops listening. */
export function subscribeActivity(onMessage: (m: ActivityMessage) => void): () => void {
  let receiver: BroadcastChannel;
  try {
    receiver = new BroadcastChannel(CHANNEL);
  } catch {
    return () => undefined;
  }
  receiver.onmessage = (e: MessageEvent<ActivityMessage>) => {
    const m = e.data;
    if (m && typeof m.source === 'string' && typeof m.phase === 'string') onMessage(m);
  };
  return () => receiver.close();
}

/**
 * For a page: tell the orb what this page is doing. Give it an id, the current phase and, if there is one, a live voice
 * level (0..1); `label` is the name the orb shows. It says idle again when the phase becomes idle and when the page is closed.
 */
export function useOrbActivity(source: string, phase: ActivityPhase, level = 0, label = source): void {
  const latest = useRef({ phase, level, label });
  latest.current = { phase, level, label };
  const lastLevelSent = useRef(0);

  // Phase changes go out at once, and repeat while the phase is not idle.
  useEffect(() => {
    publishActivity(source, phase, latest.current.level, latest.current.label);
    if (phase === 'idle') return;
    const beat = window.setInterval(() => publishActivity(source, latest.current.phase, latest.current.level, latest.current.label), HEARTBEAT_MS);
    return () => window.clearInterval(beat);
  }, [source, phase]);

  // The voice level goes out a few times a second.
  useEffect(() => {
    if (phase !== 'listening' && phase !== 'speaking') return;
    const now = Date.now();
    if (now - lastLevelSent.current < LEVEL_EVERY_MS) return;
    lastLevelSent.current = now;
    publishActivity(source, phase, level, latest.current.label);
  }, [source, phase, level]);

  // Closing the page ends what it announced.
  useEffect(() => () => publishActivity(source, 'idle'), [source]);
}

/** How a metered sound is read: fills the buffer with the samples playing now, and cleans up when the sound ends. */
interface Tap {
  read: (buf: Float32Array<ArrayBuffer>) => void;
  size: number;
  cleanup: () => void;
}

/**
 * Show a sound that is being played as "speaking", with its live loudness, for as long as it plays.
 * Returns a function that ends it. Never throws: if metering is not possible the sound still plays.
 */
function meter(source: string, makeTap: () => Tap, label: string): () => void {
  let tap: Tap | null = null;
  let raf = 0;
  let beat = 0;
  let done = false;
  let last = 0;
  let level = 0;
  const finish = () => {
    if (done) return;
    done = true;
    cancelAnimationFrame(raf);
    window.clearInterval(beat);
    try { tap?.cleanup(); } catch { /* already gone */ }
    publishActivity(source, 'idle', 0, label);
  };
  try {
    tap = makeTap();
    const buf = new Float32Array(tap.size);
    const tick = (t: number) => {
      tap!.read(buf);
      let sum = 0;
      for (const v of buf) sum += v * v;
      level = Math.min(1, Math.sqrt(sum / buf.length) * 5);
      if (t - last > LEVEL_EVERY_MS) {
        last = t;
        publishActivity(source, 'speaking', level, label);
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    beat = window.setInterval(() => publishActivity(source, 'speaking', level, label), HEARTBEAT_MS);
    publishActivity(source, 'speaking', 0, label);
  } catch {
    finish();
  }
  return finish;
}

/**
 * A page is playing speech with an <audio> element (or `new Audio(...)`): call this right after creating it, before play().
 * The sound is only observed (the element's own output is not touched), so metering can never make it silent.
 */
export function meterAudioElement(audio: HTMLAudioElement, source: string, label = source): void {
  let stop: () => void = () => undefined;
  const begin = () => {
    stop = meter(source, () => {
      const capture = (audio as HTMLAudioElement & { captureStream?: () => MediaStream }).captureStream;
      if (!capture) throw new Error('this webview cannot observe the sound');
      const ctx = new AudioContext();
      void ctx.resume().catch(() => undefined);
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 512;
      ctx.createMediaStreamSource(capture.call(audio)).connect(analyser); // not connected to the speakers
      return { read: (b) => analyser.getFloatTimeDomainData(b), size: analyser.fftSize, cleanup: () => void ctx.close().catch(() => undefined) };
    }, label);
  };
  // A stream exists only once the sound is actually playing
  if (!audio.paused && audio.readyState >= 2) begin();
  else audio.addEventListener('playing', begin, { once: true });
  for (const ev of ['ended', 'pause', 'error', 'emptied']) audio.addEventListener(ev, () => stop(), { once: true });
}

/** A page is playing speech through Web Audio: pass the context and the source node, before the node is started. */
export function meterSourceNode(playbackCtx: AudioContext, node: AudioBufferSourceNode, source: string, label = source): void {
  const stop = meter(source, () => {
    const analyser = playbackCtx.createAnalyser(); // a tap beside the page's own connection: it changes nothing that is heard
    analyser.fftSize = 512;
    node.connect(analyser);
    return { read: (b) => analyser.getFloatTimeDomainData(b), size: analyser.fftSize, cleanup: () => { try { node.disconnect(analyser); } catch { /* already gone */ } } };
  }, label);
  node.addEventListener('ended', () => stop(), { once: true });
}
