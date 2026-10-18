import { requireJson } from './api/jsonResponse.ts';

export const TTS_CHUNK_MAX_CHARS = 220;
// The CLI fallback has its own 120-second hard deadline. Give it a small
// allowance for model startup and HTTP transfer, while still bounding the UI.
export const TTS_REQUEST_TIMEOUT_MS = 150_000;

export interface SynthesizedVoiceWav {
  wav_base64: string;
  latency_ms: number;
  engine_ready: boolean;
  sample_rate: number;
}

interface FetchTtsChunkOptions {
  text: string;
  voice: string;
  lang: string;
  signal: AbortSignal;
  timeoutMs?: number;
  fetchImpl?: typeof fetch;
}

/** Fetch one bounded chunk and guarantee that a lost response cannot wait forever. */
export async function fetchTtsChunk({
  text,
  voice,
  lang,
  signal,
  timeoutMs = TTS_REQUEST_TIMEOUT_MS,
  fetchImpl = fetch,
}: FetchTtsChunkOptions): Promise<SynthesizedVoiceWav> {
  const request = new AbortController();
  let timedOut = false;
  const stopRequest = () => request.abort(signal.reason);
  signal.addEventListener('abort', stopRequest, { once: true });
  if (signal.aborted) stopRequest();
  const timeout = globalThis.setTimeout(() => {
    timedOut = true;
    request.abort(new DOMException('Voice synthesis timed out', 'TimeoutError'));
  }, timeoutMs);

  try {
    const response = await fetchImpl('/api/v1/kokoro/synthesize', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ text, voice, lang }),
      signal: request.signal,
    });
    return await requireJson<SynthesizedVoiceWav>(response, 'Voice synthesis is unavailable. Please try again.');
  } catch (error) {
    if (timedOut) throw new Error('Voice synthesis timed out. Please try again.');
    throw error;
  } finally {
    globalThis.clearTimeout(timeout);
    signal.removeEventListener('abort', stopRequest);
  }
}

/**
 * Split speech into bounded, natural-looking pieces.
 *
 * Kokoro is much more reliable on short passages. Sentence-only splitting is
 * not sufficient because pasted text, generated output, URLs, and transcripts
 * can contain thousands of characters without a sentence terminator.
 */
export function splitTtsText(text: string, maxChars = TTS_CHUNK_MAX_CHARS): string[] {
  if (!Number.isInteger(maxChars) || maxChars < 2) {
    throw new RangeError('maxChars must be an integer of at least 2');
  }

  let remaining = text.trim().replace(/\s+/gu, ' ');
  if (!remaining) return [];

  const pieces: string[] = [];
  while (Array.from(remaining).length > maxChars) {
    const window = Array.from(remaining).slice(0, maxChars);
    const minimumNaturalBreak = Math.floor(maxChars * 0.4);
    let cut = -1;

    // Prefer the end of a sentence, but do not make a tiny piece merely
    // because an abbreviation or decimal occurs near the start.
    for (let i = window.length - 1; i >= minimumNaturalBreak; i -= 1) {
      if (/[.!?;:]/u.test(window[i])) {
        cut = i + 1;
        break;
      }
    }

    // Otherwise use the last word boundary. A single overlong token is split
    // exactly at the limit; Array.from keeps surrogate pairs intact.
    if (cut < 0) {
      for (let i = window.length - 1; i >= 0; i -= 1) {
        if (/\s/u.test(window[i])) {
          cut = i;
          break;
        }
      }
    }
    if (cut <= 0) cut = window.length;

    pieces.push(window.slice(0, cut).join('').trim());
    remaining = Array.from(remaining).slice(cut).join('').trimStart();
  }

  if (remaining) pieces.push(remaining);
  return pieces;
}
