import assert from 'node:assert/strict';
import test from 'node:test';
import { classifyCompanyQuestion, composeCompanyAnswer, searchNeedle } from '../companyBrain.ts';
import { trustedDesktopOrigin } from '../falkorPlugin.ts';

const rows = [
  {
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
    source_updated_at: 10,
  },
  {
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
    source_id: 'SRC-CHAT-221',
    source_title: 'Identity incident handoff',
    source_kind: 'chat',
    source_uri: 'company://chat/identity/221',
    source_updated_at: 20,
  },
];

test('routes company questions to the relevant graph intent', () => {
  assert.equal(classifyCompanyQuestion('Who owns the OAuth service?'), 'ownership');
  assert.equal(classifyCompanyQuestion('Why did we choose PKCE?'), 'decision');
  assert.equal(classifyCompanyQuestion('Summarize Identity Platform'), 'project_context');
});

test('uses an entity word as the graph search needle', () => {
  assert.equal(searchNeedle('Who should handle the OAuth production incident?'), 'oauth');
});

test('composes an answer, action, sources, and explanation paths only from graph rows', () => {
  const answer = composeCompanyAnswer('Who should handle this?', rows);
  assert.ok(answer);
  assert.match(answer.answer, /Priya Raman/);
  assert.match(answer.answer, /DEC-014/);
  assert.equal(answer.action?.ticketId, 'INC-1042');
  assert.deepEqual(answer.sources.map((source) => source.id), ['SRC-ADR-014', 'SRC-CHAT-221']);
  assert.deepEqual(answer.paths[0], ['INC-1042', 'BELONGS_TO', 'Identity Platform', 'OWNED_BY', 'Priya Raman']);
});

test('refuses to fabricate an answer when the graph returned no connected records', () => {
  assert.equal(composeCompanyAnswer('Who owns it?', []), null);
});

test('CORS echoes only an explicitly trusted Aqua desktop origin', () => {
  assert.equal(trustedDesktopOrigin({ headers: { origin: 'http://127.0.0.1:1420' } }), 'http://127.0.0.1:1420');
  assert.equal(trustedDesktopOrigin({ headers: { origin: 'http://localhost:1420' } }), 'http://localhost:1420');
  assert.equal(trustedDesktopOrigin({ headers: { origin: 'https://attacker.example' } }), null);
  assert.equal(trustedDesktopOrigin({ headers: {} }), null);
});
