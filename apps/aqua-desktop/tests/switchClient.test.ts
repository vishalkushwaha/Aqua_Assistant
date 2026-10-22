import assert from 'node:assert/strict';
import test from 'node:test';
import {
  registerSwitchAgent,
  SwitchClient,
  SwitchProtocolError,
} from '../switch/switchClient.ts';

test('SwitchProtocolError includes HTTP status and error code', () => {
  const err = new SwitchProtocolError('Unauthorized access to Switch', 401, 'INVALID_TOKEN');
  assert.equal(err.name, 'SwitchProtocolError');
  assert.equal(err.message, 'Unauthorized access to Switch');
  assert.equal(err.status, 401);
  assert.equal(err.code, 'INVALID_TOKEN');
});

test('SwitchClient validates bridge URL protocol', () => {
  assert.throws(
    () => new SwitchClient({
      bridgeUrl: 'ftp://localhost:8080',
      agentId: 'agent_1',
      apiKey: 'key_1',
    }),
    /Switch bridge URL must use http or https/,
  );

  assert.throws(
    () => new SwitchClient({
      bridgeUrl: 'not-a-valid-url',
      agentId: 'agent_1',
      apiKey: 'key_1',
    }),
    /Invalid URL/,
  );

  const client = new SwitchClient({
    bridgeUrl: 'http://127.0.0.1:8080/',
    agentId: 'agent_1',
    apiKey: 'key_1',
  });
  assert.ok(client.connectionId);
});

test('registerSwitchAgent sends formatted registration request', async () => {
  let capturedUrl = '';
  let capturedHeaders: Record<string, string> = {};
  let capturedBody: any = null;

  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    capturedUrl = String(input);
    capturedHeaders = (init?.headers as Record<string, string>) || {};
    capturedBody = JSON.parse(String(init?.body || '{}'));

    return new Response(JSON.stringify({ id: 'agent_aqua_42', api_key: 'sec_test_token' }), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  }) as typeof fetch;

  try {
    const reg = await registerSwitchAgent('http://127.0.0.1:8080', 'reg_token_secret', {
      name: 'aqua-company-brain',
      displayName: 'Aqua Company Brain',
      description: 'Grounded enterprise agent',
      connectorType: 'aqua-agent',
      overwrite: true,
    });

    assert.equal(reg.id, 'agent_aqua_42');
    assert.equal(reg.apiKey, 'sec_test_token');
    assert.equal(capturedUrl, 'http://127.0.0.1:8080/agents');
    assert.equal(capturedHeaders['Authorization'], 'Bearer reg_token_secret');
    assert.equal(capturedHeaders['Content-Type'], 'application/json');
    assert.equal(capturedBody.name, 'aqua-company-brain');
    assert.equal(capturedBody.overwrite, true);
    assert.equal(capturedBody.connector_type, 'aqua-agent');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('registerSwitchAgent throws SwitchProtocolError on bridge error', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => {
    return new Response(JSON.stringify({ detail: 'Token has expired', code: 'TOKEN_EXPIRED' }), {
      status: 403,
      headers: { 'Content-Type': 'application/json' },
    });
  }) as typeof fetch;

  try {
    await assert.rejects(
      async () => {
        await registerSwitchAgent('http://127.0.0.1:8080', 'expired_token', {
          name: 'aqua-agent',
          description: 'test',
          connectorType: 'test',
        });
      },
      (err: any) => {
        assert.ok(err instanceof SwitchProtocolError);
        assert.equal(err.status, 403);
        assert.equal(err.code, 'TOKEN_EXPIRED');
        assert.equal(err.message, 'Token has expired');
        return true;
      },
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});
