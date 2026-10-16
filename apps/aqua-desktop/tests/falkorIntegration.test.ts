import assert from 'node:assert/strict';
import test from 'node:test';

const enabled = process.env.TEST_FALKORDB === '1';
const baseUrl = process.env.TEST_FALKORDB_BASE_URL || 'http://127.0.0.1:1420';
const trustedOrigin = process.env.TEST_FALKORDB_ORIGIN || 'http://127.0.0.1:1420';

interface JsonResponse {
  response: Response;
  body: any;
}

async function request(path: string, init: RequestInit = {}): Promise<JsonResponse> {
  const headers = new Headers(init.headers);
  headers.set('Accept', 'application/json');
  headers.set('Origin', trustedOrigin);
  if (init.body) headers.set('Content-Type', 'application/json');
  const response = await fetch(`${baseUrl}${path}`, { ...init, headers });
  const body = await response.json();
  return { response, body };
}

function post(path: string, body?: unknown): Promise<JsonResponse> {
  return request(path, {
    method: 'POST',
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

test('real FalkorDB workflow: seed, explain, act, remember, and fail closed', { skip: !enabled, timeout: 45_000 }, async () => {
  const status = await request('/api/falkordb/status');
  assert.equal(status.response.status, 200);
  assert.equal(status.body.connected, true, `FalkorDB unavailable: ${status.body.error || 'unknown error'}`);

  const seeded = await post('/api/falkordb/company/seed');
  assert.equal(seeded.response.status, 200);
  assert.equal(seeded.body.success, true);

  const asked = await post('/api/falkordb/company/ask', {
    question: 'Who should handle the OAuth production incident and why?',
    tenantId: 'demo',
  });
  assert.equal(asked.response.status, 200);
  assert.equal(asked.body.success, true);
  assert.equal(asked.body.graphRequired, true);
  assert.equal(asked.body.sessionId, 'session_demo_primary');
  assert.match(asked.body.answerId, /^answer_/);
  assert.ok(Array.isArray(asked.body.paths) && asked.body.paths.length >= 3);
  assert.ok(Array.isArray(asked.body.sources) && asked.body.sources.length >= 2);
  assert.equal(asked.body.action?.kind, 'assign_ticket');

  const acted = await post('/api/falkordb/company/action', { ...asked.body.action, tenantId: 'demo' });
  assert.equal(acted.response.status, 200);
  assert.equal(acted.body.success, true);

  // This second request proves that the prior question and action live in FalkorDB,
  // rather than only in the bridge process's JavaScript memory.
  const recalled = await post('/api/falkordb/query', {
    cypher: `
      MATCH (q:Question {id: $questionId, tenant_id: $tenantId})-[:ANSWERED_WITH]->(source:Source)
      MATCH (action:Action {id: $actionId, tenant_id: $tenantId})-[:ACTED_ON]->(ticket:Ticket)
      RETURN q.id AS question_id, collect(DISTINCT source.id) AS source_ids,
        action.id AS action_id, ticket.id AS ticket_id
    `,
    params: { questionId: asked.body.interactionId, actionId: acted.body.actionId, tenantId: 'demo' },
  });
  assert.equal(recalled.response.status, 200);
  assert.equal(recalled.body.success, true);
  assert.equal(recalled.body.data.length, 1);
  assert.equal(recalled.body.data[0].question_id, asked.body.interactionId);
  assert.equal(recalled.body.data[0].action_id, acted.body.actionId);
  assert.ok(recalled.body.data[0].source_ids.length >= 2);

  const memory = await post('/api/falkordb/company/memory', {
    tenantId: 'demo',
    sessionId: asked.body.sessionId,
  });
  assert.equal(memory.response.status, 200);
  assert.ok(memory.body.questions.some((entry: any) => entry.questionId === asked.body.interactionId));
  assert.ok(memory.body.actions.some((entry: any) => entry.actionId === acted.body.actionId));

  const handedOff = await post('/api/falkordb/company/handoff', {
    tenantId: 'demo',
    sessionId: asked.body.sessionId,
    summary: 'Continue incident response using the cited sources.',
  });
  assert.equal(handedOff.response.status, 200);
  assert.equal(handedOff.body.success, true);

  const coordinated = await post('/api/falkordb/company/memory', {
    tenantId: 'demo',
    sessionId: asked.body.sessionId,
  });
  assert.ok(coordinated.body.handoffs.some((entry: any) => entry.handoffId === handedOff.body.handoffId));

  const rejected = await post('/api/falkordb/company/action', {
    kind: 'assign_ticket',
    ticketId: 'INC-1042',
    assigneeId: 'person_not_an_owner',
    tenantId: 'demo',
  });
  assert.equal(rejected.response.status, 409);
  assert.equal(rejected.body.success, false);

  // Temporarily point only this development bridge at a closed local port. The
  // original non-secret settings are restored even if the assertion fails.
  try {
    const disconnected = await post('/api/falkordb/config', { port: 1 });
    assert.equal(disconnected.response.status, 200);
    const refused = await post('/api/falkordb/company/ask', {
      question: 'Who owns the OAuth incident?',
      tenantId: 'demo',
    });
    assert.equal(refused.response.status, 503);
    assert.equal(refused.body.success, false);
    assert.equal(refused.body.graphRequired, true);
    assert.equal('answer' in refused.body, false);
  } finally {
    await post('/api/falkordb/config', {
      host: status.body.host,
      port: status.body.port,
      username: status.body.username,
      graphName: status.body.graphName,
    });
  }
});
