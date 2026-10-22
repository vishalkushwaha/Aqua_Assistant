import assert from 'node:assert/strict';
import test from 'node:test';
import {
  CompanyBrainRepository,
  type CompanyGraphQuery,
  type GraphQueryResult,
} from '../companyBrainRepository.ts';

const evidence = {
  ticket_id: 'INC-1042',
  ticket_title: 'OAuth production incident',
  project_id: 'project_identity',
  project_name: 'Identity Platform',
  owner_id: 'person_priya',
  owner_name: 'Priya Raman',
  owner_role: 'Staff Engineer',
  team_name: 'Identity Engineering',
  decision_id: 'DEC-014',
  decision_title: 'Use staged PKCE migration',
  decision_summary: 'Roll out PKCE by client cohort.',
  source_id: 'SRC-ADR-014',
  source_title: 'ADR-014: OAuth PKCE migration',
  source_kind: 'document',
  source_uri: 'company://docs/ADR-014',
  source_updated_at: 20,
};

interface RecordedCall {
  cypher: string;
  params: Record<string, unknown>;
}

function repositoryWith(results: GraphQueryResult[], now = 1234) {
  const calls: RecordedCall[] = [];
  const query: CompanyGraphQuery = async (_graph, cypher, options) => {
    calls.push({ cypher, params: options?.params || {} });
    return results.shift() || {};
  };
  return {
    calls,
    repository: new CompanyBrainRepository(async () => ({ connected: true }), query, () => now),
  };
}

test('repository seeds the existing repeatable company dataset', async () => {
  const { repository, calls } = repositoryWith([{ data: [{ company_id: 'company_acme' }], statistics: { nodes_created: 1 } }]);
  const result = await repository.seed();

  assert.deepEqual(result.data, [{ company_id: 'company_acme' }]);
  assert.deepEqual(result.statistics, { nodes_created: 1 });
  assert.match(calls[0].cypher, /MERGE \(company:Company/);
  assert.match(calls[0].cypher, /MERGE \(ticket\)-\[:BELONGS_TO\]->\(project\)/);
});

test('repository retries a natural question through FalkorDB and persists only grounded evidence', async () => {
  const { repository, calls } = repositoryWith([
    { data: [] },
    { data: [evidence] },
    { data: [{ interaction_id: 'question_1234', answer_id: 'answer_1234' }] },
  ]);

  const answer = await repository.ask('Who should handle the OAuth incident?', 'tenant-one', 'switch-room-one');

  assert.ok(answer);
  assert.equal(answer.interactionId, 'question_1234');
  assert.equal(answer.answerId, 'answer_1234');
  assert.equal(answer.sessionId, 'switch-room-one');
  assert.equal(answer.graphRequired, true);
  assert.equal(calls.length, 3);
  assert.equal(calls[0].params.needle, 'oauth');
  assert.equal(calls[1].params.needle, '');
  assert.deepEqual(calls[2].params.sourceIds, ['SRC-ADR-014']);
  assert.equal(calls[2].params.tenantId, 'tenant-one');
});

test('repository fails closed without persisting an ungrounded answer', async () => {
  const { repository, calls } = repositoryWith([{ data: [] }, { data: [] }]);

  const answer = await repository.ask('What is our undocumented plan?', 'demo', 'session');

  assert.equal(answer, null);
  assert.equal(calls.length, 2);
});

test('repository refuses an unvalidated assignment and returns a stable action id for a valid one', async () => {
  const refused = repositoryWith([{ data: [] }], 2000);
  assert.equal(await refused.repository.assignTicket('INC-1042', 'not-owner', 'demo', 'session'), null);

  const accepted = repositoryWith([{ data: [{ action_id: 'action_2000', ticket_id: 'INC-1042', assignee: 'Priya Raman' }] }], 2000);
  const result = await accepted.repository.assignTicket('INC-1042', 'person_priya', 'demo', 'session');

  assert.equal(result?.actionId, 'action_2000');
  assert.equal(accepted.calls[0].params.outcomeId, 'outcome_2000');
  assert.match(accepted.calls[0].cypher, /MATCH \(person:Person .*\)-\[:OWNS\]->\(project\)/s);
});

test('repository maps memory and bounds handoff summaries without HTTP dependencies', async () => {
  const { repository, calls } = repositoryWith([
    { data: [{ question_id: 'q1', question: 'Who owns it?', answer_id: 'a1', answer: 'Priya', source_ids: ['s1'] }] },
    { data: [{ action_id: 'act1', kind: 'assign_ticket', status: 'completed', target_id: 'INC-1042' }] },
    { data: [{ handoff_id: 'h1', from_agent: 'Aqua', to_agent: 'Incident', summary: 'Investigate' }] },
    { data: [{ handoff_id: 'handoff_3000' }] },
  ], 3000);

  const memory = await repository.recallMemory('demo', 'switch-room');
  assert.equal(memory.questions[0].questionId, 'q1');
  assert.equal(memory.actions[0].targetId, 'INC-1042');
  assert.equal(memory.handoffs[0].handoffId, 'h1');

  const handoffId = await repository.createHandoff('demo', 'switch-room', 'x'.repeat(32_100));
  assert.equal(handoffId, 'handoff_3000');
  assert.equal(String(calls[3].params.summary).length, 32_000);
});
