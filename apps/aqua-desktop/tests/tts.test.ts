import assert from 'node:assert/strict';
import test from 'node:test';
import { fetchTtsChunk, splitTtsText, TTS_CHUNK_MAX_CHARS } from '../src/tts.ts';

test('keeps short speech in one piece', () => {
  assert.deepEqual(splitTtsText('  Hello, world.  '), ['Hello, world.']);
});

test('bounds a long unpunctuated prompt instead of sending it as one request', () => {
  const input = Array.from({ length: 300 }, (_, i) => `word${i}`).join(' ');
  const pieces = splitTtsText(input);

  assert.ok(pieces.length > 1);
  assert.ok(pieces.every((piece) => Array.from(piece).length <= TTS_CHUNK_MAX_CHARS));
  assert.equal(pieces.join(' '), input);
});

test('splits an overlong token safely without breaking unicode characters', () => {
  const input = '🌊'.repeat(501);
  const pieces = splitTtsText(input, 100);

  assert.deepEqual(pieces.map((piece) => Array.from(piece).length), [100, 100, 100, 100, 100, 1]);
  assert.equal(pieces.join(''), input);
});

test('prefers sentence boundaries when they are available', () => {
  const first = 'This is the first complete sentence.';
  const input = `${first} ${'continuation '.repeat(20)}`;
  const pieces = splitTtsText(input, 80);

  assert.equal(pieces[0], first);
  assert.ok(pieces.every((piece) => Array.from(piece).length <= 80));
});

test('empty and whitespace-only input produces no requests', () => {
  assert.deepEqual(splitTtsText(' \n\t '), []);
});

test('a synthesis request that never responds is aborted at its deadline', async () => {
  const fetchThatNeverResponds: typeof fetch = (_input, init) => new Promise((_resolve, reject) => {
    init?.signal?.addEventListener('abort', () => reject(init.signal?.reason), { once: true });
  });

  await assert.rejects(
    fetchTtsChunk({
      text: 'A bounded chunk',
      voice: 'af_sarah',
      lang: 'en-us',
      signal: new AbortController().signal,
      timeoutMs: 20,
      fetchImpl: fetchThatNeverResponds,
    }),
    /timed out/i,
  );
});

test('stopping a synthesis aborts its in-flight request without reporting a timeout', async () => {
  const fetchThatNeverResponds: typeof fetch = (_input, init) => new Promise((_resolve, reject) => {
    init?.signal?.addEventListener('abort', () => reject(new DOMException('stopped', 'AbortError')), { once: true });
  });
  const controller = new AbortController();
  const request = fetchTtsChunk({
    text: 'Stop this chunk',
    voice: 'af_sarah',
    lang: 'en-us',
    signal: controller.signal,
    timeoutMs: 1_000,
    fetchImpl: fetchThatNeverResponds,
  });
  controller.abort();

  await assert.rejects(request, (error: unknown) => error instanceof DOMException && error.name === 'AbortError');
});
