// Speaks the agent's answers aloud.
//
// Only the local Kokoro engine (through the Aqua core) speaks, so the text never leaves this PC and no other voice is
// ever used. If Kokoro is not ready nothing is spoken, and the caller is told so.

import { meterAudioElement } from '../activity';
import { requireJson } from './jsonResponse.ts';

const VOICE_KEY = 'aqua.agent.speak';
const MAX_SPOKEN_CHARS = 700;
const CHUNK_CHARS = 220;
const FIRST_CHUNK_CHARS = 100; // the first piece is short, so speech starts after one sentence has been made, not three

export function loadSpeakSetting(): boolean {
  try {
    return window.localStorage.getItem(VOICE_KEY) !== 'off';
  } catch {
    return true;
  }
}

export function saveSpeakSetting(on: boolean): void {
  try {
    window.localStorage.setItem(VOICE_KEY, on ? 'on' : 'off');
  } catch {
    /* storage can be blocked; the toggle still works for this session */
  }
}

/** Turns an answer into what a person would say: no markdown, links, code or symbols read aloud. */
export function toSpokenText(answer: string): string {
  let text = answer
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/https?:\/\/\S+/g, ' ')
    .replace(/^\s{0,3}#{1,6}\s+/gm, '')
    .replace(/^\s*[-*+]\s+/gm, '')
    .replace(/[*_~>|]+/g, ' ')
    .replace(/(\d)\s?°\s?C\b/g, '$1 degrees Celsius')
    .replace(/(\d)\s?°\s?F\b/g, '$1 degrees Fahrenheit')
    .replace(/°/g, ' degrees ')
    .replace(/\s+/g, ' ')
    .trim();
  if (text.length > MAX_SPOKEN_CHARS) {
    const cut = text.slice(0, MAX_SPOKEN_CHARS);
    const end = Math.max(cut.lastIndexOf('. '), cut.lastIndexOf('! '), cut.lastIndexOf('? '));
    text = (end > 200 ? cut.slice(0, end + 1) : cut).trim();
  }
  return text;
}

/** Sentence-sized pieces so the first one starts playing while the rest are still being made. */
export function splitForSpeech(text: string): string[] {
  const sentences = text.match(/[^.!?]+[.!?]+["')\]]*\s*|[^.!?]+$/g) ?? [text];
  const chunks: string[] = [];
  let current = '';
  for (const raw of sentences) {
    const s = raw.trim();
    if (!s) continue;
    const limit = chunks.length === 0 ? FIRST_CHUNK_CHARS : CHUNK_CHARS;
    if (current && current.length + s.length + 1 > limit) {
      chunks.push(current);
      current = s;
    } else {
      current = current ? `${current} ${s}` : s;
    }
  }
  if (current) chunks.push(current);
  return chunks;
}

let generation = 0;
let currentAudio: HTMLAudioElement | null = null;

export function stopSpeaking(): void {
  generation += 1;
  if (currentAudio) {
    currentAudio.pause();
    currentAudio = null;
  }
}

/**
 * Both kinds of speech (answers, and device announcements) play their audio inside this lock, so the two never overlap,
 * whichever window they are in. Answers hold it for one piece at a time, so a critical announcement can come between pieces.
 */
export async function withAudioLock<T>(fn: () => Promise<T>): Promise<T> {
  const locks = (navigator as Navigator & { locks?: LockManager }).locks;
  if (!locks) return fn();
  return (await locks.request('aqua-audio-output', () => fn())) as T;
}

/** Make speech with the local Kokoro engine. The WAV as base64, or null when it is not ready. */
export function synthesizeWav(text: string, signal: AbortSignal): Promise<string | null> {
  return synthesize(text, signal);
}

async function synthesize(text: string, signal: AbortSignal): Promise<string | null> {
  try {
    const res = await fetch('/api/v1/kokoro/synthesize', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ text, voice: 'af_sarah', lang: 'en-us' }),
      signal,
    });
    if (!res.ok) return null;
    const data = await requireJson<{ wav_base64?: string; engine_ready?: boolean }>(res);
    return data.engine_ready && data.wav_base64 ? data.wav_base64 : null;
  } catch {
    return null;
  }
}

function playWav(b64: string, myGeneration: number): Promise<void> {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const url = URL.createObjectURL(new Blob([bytes], { type: 'audio/wav' }));
  const audio = new Audio(url);
  meterAudioElement(audio, 'agent-speech', 'Agent reply'); // the voice orb follows the reply as it plays
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
      // stopSpeaking() pauses it: end quietly instead of leaving the queue waiting.
      if (myGeneration !== generation) {
        done();
        resolve();
      }
    };
    audio.onerror = () => {
      done();
      reject(new Error('Audio playback failed'));
    };
    audio.play().catch((e) => {
      done();
      reject(e);
    });
  });
}

/** Speaks `answer`. Resolves when it has finished or was stopped; never throws. Returns 'kokoro' when it was spoken, 'none' when it was not (Kokoro not ready, or nothing to say). */
export async function speakAnswer(answer: string): Promise<'kokoro' | 'none'> {
  stopSpeaking();
  const mine = generation;
  const text = toSpokenText(answer);
  if (!text) return 'none';
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), 60_000);
  try {
    const chunks = splitForSpeech(text);
    // Make the next piece while the current one plays.
    let next: Promise<string | null> = synthesize(chunks[0], controller.signal);
    for (let i = 0; i < chunks.length; i++) {
      const wav = await next;
      if (mine !== generation) return 'kokoro';
      if (!wav) {
        return i === 0 ? 'none' : 'kokoro'; // Kokoro is not ready: say nothing rather than use another voice
      }
      if (i + 1 < chunks.length) next = synthesize(chunks[i + 1], controller.signal);
      await withAudioLock(() => playWav(wav, mine));
      if (mine !== generation) return 'kokoro';
      if (i === chunks.length - 1) return 'kokoro';
    }
  } catch {
    if (mine !== generation) return 'none';
  } finally {
    window.clearTimeout(timer);
  }
  return 'none';
}
