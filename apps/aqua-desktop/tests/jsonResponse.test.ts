import assert from 'node:assert/strict';
import test from 'node:test';
import { JsonResponseError, readResponseJson, requireJson, responseErrorMessage } from '../src/api/jsonResponse.ts';

test('required JSON accepts valid JSON and a UTF-8 BOM', async () => {
  const response = new Response('\uFEFF{"ok":true}', { status: 200 });
  assert.deepEqual(await requireJson<{ ok: boolean }>(response), { ok: true });
});

test('malformed success never leaks a JSON SyntaxError', async () => {
  const response = new Response('<!doctype html><title>proxy error</title>', { status: 200 });
  await assert.rejects(() => requireJson(response), (error: unknown) => {
    assert.ok(error instanceof JsonResponseError);
    assert.equal(error.message, 'Aqua sent a response the app could not read. Please try again.');
    assert.doesNotMatch(error.message, /unexpected token|doctype|syntaxerror/i);
    return true;
  });
});

test('empty success has a stable page-safe error', async () => {
  await assert.rejects(() => requireJson(new Response(null, { status: 204 })), /Aqua sent an empty response\. Please try again\./);
});

test('error JSON uses a short server sentence and malformed errors use the fallback', async () => {
  await assert.rejects(
    () => requireJson(new Response('{"error":"Database is still starting"}', { status: 503 }), 'Could not load this page.'),
    /Database is still starting/,
  );
  await assert.rejects(
    () => requireJson(new Response('<html>secret proxy page</html>', { status: 502 }), 'Could not load this page.'),
    (error: unknown) => error instanceof JsonResponseError
      && error.message === 'Aqua Core is still starting. This panel will reconnect automatically.',
  );
});

test('client errors keep the page-specific fallback without exposing an HTTP code', async () => {
  await assert.rejects(
    () => requireJson(new Response('', { status: 400 }), 'That setting could not be saved.'),
    (error: unknown) => error instanceof JsonResponseError
      && error.message === 'That setting could not be saved.'
      && !error.message.includes('400'),
  );
});

test('optional reader handles empty bodies and message extraction rejects raw objects', async () => {
  assert.equal(await readResponseJson(new Response('', { status: 200 })), null);
  assert.equal(responseErrorMessage({ message: ' Try again. ' }), 'Try again.');
  assert.equal(responseErrorMessage({ error: { nested: true } }), '');
});
