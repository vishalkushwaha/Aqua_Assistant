// Streamed turns: the core answers POST /api/v1/pipeline/text/stream and /api/v1/pipeline/wav/stream with server-sent events, so
// the first sentence of an answer is heard while the rest is still being written (a short answer takes 10 s to write on a laptop, its
// first sentence 2 or 3). The events:
//
//   heard   {"text": "what is the date"}                              voice only, once, as soon as the words are known
//   speech  {"index": 0, "text": "Today is...", "wav_base64": "..."}  one per spoken piece, in order (the sound is empty when
//                                                                     speech could not be made: the words still arrive)
//   done    the JSON the plain endpoint returns, with an empty tts_wav_base64, plus "first_audio_ms"
//   error   {"error": "...", "status": 503}                          the turn failed; no `done` follows
//
// Hanging up (aborting the request) stops the turn on the core.

export interface SpokenPiece {
  index: number;
  text: string;
  /** A WAV file as base64; empty when the core could not make speech for this piece. */
  wav_base64: string;
}

export interface StreamHandlers {
  /** What was recognised, as soon as it is known (voice turns only). */
  onHeard?: (text: string) => void;
  /**
   * One spoken piece of the answer, in order. Pieces keep arriving while this one is being played: return a promise (the playback) and
   * the next piece waits for it to settle, so pieces play one after the other and never over each other.
   */
  onSpeech?: (piece: SpokenPiece) => void | Promise<void>;
  /** The turn's result, the moment it arrives: the last pieces may still be playing (the returned promise waits for them). */
  onDone?: (result: StreamedResult) => void;
}

export interface StreamOptions {
  /**
   * The ordinary endpoint to use when the core has no streaming one (an older core answers 404). Its single reply is handed to
   * `onSpeech` as one piece (its whole sound) and returned like the final event of a stream.
   */
  fallbackPath?: string;
  /** For tests: the `fetch` to use. */
  fetchImpl?: typeof fetch;
}

/** The data of the final `done` event: the same fields as the ordinary endpoint's reply (`assistant_text`, `transcribed_text`, ...). */
export type StreamedResult = Record<string, any>;

/** Cut a stream of text into server-sent events, however it was cut into chunks. Comment lines (": keep-alive") are not events. */
export async function* readEvents(body: ReadableStream<Uint8Array>): AsyncGenerator<{ event: string; data: string }> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      // A multi-byte character cut between two chunks is held by the decoder until it is whole.
      buffer = (buffer + decoder.decode(value, { stream: true })).replace(/\r\n/g, '\n');
      for (let end = buffer.indexOf('\n\n'); end !== -1; end = buffer.indexOf('\n\n')) {
        const block = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const event = parseEvent(block);
        if (event) yield event;
      }
    }
  } finally {
    reader.releaseLock();
  }
}

function parseEvent(block: string): { event: string; data: string } | null {
  let event = 'message';
  const data: string[] = [];
  for (const line of block.split('\n')) {
    if (!line || line.startsWith(':')) continue;
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') event = value;
    else if (field === 'data') data.push(value);
  }
  return data.length ? { event, data: data.join('\n') } : null;
}

function parseData(data: string): StreamedResult {
  try {
    const parsed = JSON.parse(data);
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

async function failure(res: Response): Promise<Error> {
  let why = `${res.status} ${res.statusText}`;
  try {
    const body = await res.json();
    if (body?.error) why = String(body.error);
  } catch {
    /* not JSON */
  }
  return new Error(why);
}

/**
 * Run one turn on a streaming endpoint. The pieces of the answer go to `handlers` as they arrive; the result is the final `done` data,
 * returned once every piece has been played. A failed turn rejects with the core's message, after the pieces that arrived have been played.
 */
export async function streamTurn(path: string, request: unknown, signal: AbortSignal | undefined, handlers: StreamHandlers, options: StreamOptions = {}): Promise<StreamedResult> {
  const doFetch = options.fetchImpl ?? fetch;
  const post = (url: string, accept: string) =>
    doFetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json', Accept: accept }, body: JSON.stringify(request), signal });

  const res = await post(path, 'text/event-stream');
  if (res.status === 404 && options.fallbackPath) {
    const plain = await post(options.fallbackPath, 'application/json');
    if (!plain.ok) throw await failure(plain);
    const data = (await plain.json()) as StreamedResult;
    const result = data.result ?? data;
    handlers.onHeard?.(String(result.transcribed_text ?? ''));
    handlers.onDone?.(data);
    if (result.tts_wav_base64) await handlers.onSpeech?.({ index: 0, text: String(result.assistant_text ?? ''), wav_base64: String(result.tts_wav_base64) });
    return data;
  }
  if (!res.ok) throw await failure(res);
  if (!res.body) throw new Error('this browser cannot read a streamed answer');

  // The pieces play one after the other, while the stream goes on being read.
  let playing: Promise<void> = Promise.resolve();
  let done: StreamedResult | null = null;
  try {
    for await (const { event, data } of readEvents(res.body)) {
      const payload = parseData(data);
      if (event === 'heard') {
        handlers.onHeard?.(String(payload.text ?? ''));
      } else if (event === 'speech') {
        const piece: SpokenPiece = { index: Number(payload.index ?? 0), text: String(payload.text ?? ''), wav_base64: String(payload.wav_base64 ?? '') };
        // A piece that is still waiting when the page is told to stop (Stop, a newer request) is dropped, not played: an aborted signal never
        // fires its listener again, so a piece started after the abort would play in full.
        playing = playing.then(() => (signal?.aborted ? undefined : handlers.onSpeech?.(piece)));
      } else if (event === 'done') {
        done = payload;
        handlers.onDone?.(payload);
        break;
      } else if (event === 'error') {
        throw new Error(String(payload.error ?? 'the request failed'));
      }
    }
  } catch (e) {
    // After a failure what was said stays said: the pieces already received finish playing before it is reported. After a Stop nothing more
    // is played, and the abort is reported at once (the piece that is playing ends by the same signal).
    if (!signal?.aborted) await playing.catch(() => undefined);
    throw e;
  }
  await playing;
  if (!done) throw new Error('the answer stopped arriving before it was finished');
  return done;
}
