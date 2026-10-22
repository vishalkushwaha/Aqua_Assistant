import assert from 'node:assert/strict';
import { rmSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import type { CompanyAnswer, CompanyBrainRepository, CompanyMemory } from '../companyBrainRepository.ts';
import { CompanyBrainSwitchWorkflow } from '../switch/companyBrainWorkflow.ts';
import type { SwitchClient, SwitchDomainEvent } from '../switch/switchClient.ts';
import { SwitchWorkflowState } from '../switch/workflowState.ts';

const TEST_STATE_FILE = resolve(process.cwd(), '.test-switch-workflow-events.json');

function cleanup() {
  try {
    rmSync(TEST_STATE_FILE, { force: true });
  } catch {
    // ignore
  }
}

interface MockPost {
  roomId: string;
  content: string;
}

function createMockClient() {
  const posted: MockPost[] = [];
  const client = {
    postMessage: async (roomId: string, content: string) => {
      posted.push({ roomId, content });
      return { success: true };
    },
  } as unknown as SwitchClient;
  return { client, posted };
}

function makeMessageEvent(body: string, messageId = 'msg_1', roomId = 'room_incident'): SwitchDomainEvent {
  return {
    event: 'message',
    id: 1,
    data: {
      type: 'message',
      room_id: roomId,
      bridge_id: 'bridge_local',
      channel_type: 'mattermost',
      sequence: 1,
      payload: {
        addressed: true,
        sender: 'user_sam',
        sender_name: 'Sam',
        message_id: messageId,
        body,
        timestamp: 1760000000000,
        thread_id: null,
        attachments: [],
      },
    },
  };
}

const mockAnswer: CompanyAnswer = {
  interactionId: 'q_1',
  answerId: 'ans_1',
  sessionId: 'switch_room_room_incident',
  graphRequired: true,
  intent: 'ownership',
  answer: 'Priya Raman owns Identity Platform and should handle the incident. Earlier decision DEC-014 applies.',
  facts: {
    ticketId: 'INC-1042',
    ticketTitle: 'OAuth production incident',
    projectName: 'Identity Platform',
    ownerName: 'Priya Raman',
    ownerRole: 'Staff Engineer',
    teamName: 'Identity Engineering',
    decisionId: 'DEC-014',
    decisionTitle: 'Use staged PKCE migration',
    decisionSummary: 'Preserve legacy callback.',
  },
  action: {
    kind: 'assign_ticket',
    ticketId: 'INC-1042',
    assigneeId: 'person_priya',
    authorized: true,
    label: 'Assign INC-1042 to Priya Raman',
  },
  paths: [
    ['INC-1042', 'BELONGS_TO', 'Identity Platform', 'OWNS', 'Priya Raman'],
    ['DEC-014', 'AFFECTS', 'Identity Platform'],
  ],
  sources: [
    { id: 'SRC-ADR-014', title: 'ADR-014: OAuth PKCE migration', uri: 'company://docs/ADR-014', kind: 'document' },
  ],
};

test('Switch workflow prompts user with guidance on empty mention', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {} as CompanyBrainRepository;
    const workflow = new CompanyBrainSwitchWorkflow({ repository, state });

    await workflow.handle(makeMessageEvent('@aqua-company-brain'), client);

    assert.equal(posted.length, 1);
    assert.match(posted[0].content, /Ask me who owns an incident/);
    assert.ok(state.wasProcessed('msg_1'));
  } finally {
    cleanup();
  }
});

test('Switch workflow returns grounded answer and proposes assignment', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {
      ask: async () => mockAnswer,
    } as unknown as CompanyBrainRepository;

    const workflow = new CompanyBrainSwitchWorkflow({
      repository,
      state,
      agentName: 'aqua-company-brain',
    });

    await workflow.handle(
      makeMessageEvent('@aqua-company-brain Who should handle the OAuth production incident?'),
      client,
    );

    assert.equal(posted.length, 1);
    const text = posted[0].content;
    assert.match(text, /Priya Raman owns Identity Platform/);
    assert.match(text, /Evidence paths:/);
    assert.match(text, /SRC-ADR-014: ADR-014: OAuth PKCE migration/);
    assert.match(text, /Proposed action: Assign INC-1042 to Priya Raman/);
    assert.match(text, /reply `approve assignment INC-1042`/i);

    // Verify pending assignment stored in state
    const pending = state.pendingAssignment('room_incident');
    assert.ok(pending);
    assert.equal(pending.ticketId, 'INC-1042');
    assert.equal(pending.assigneeId, 'person_priya');
  } finally {
    cleanup();
  }
});

test('Switch workflow approves assignment, creates handoff, and mentions peer agent', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    let assigned = false;
    let handoffCreated = false;

    const repository = {
      assignTicket: async (ticketId: string, assigneeId: string) => {
        if (ticketId === 'INC-1042' && assigneeId === 'person_priya') {
          assigned = true;
          return { actionId: 'ACT-9001' };
        }
        return null;
      },
      createHandoff: async () => {
        handoffCreated = true;
        return 'HANDOFF-42';
      },
    } as unknown as CompanyBrainRepository;

    const workflow = new CompanyBrainSwitchWorkflow({
      repository,
      state,
      agentName: 'aqua-company-brain',
      incidentAgentName: 'release-agent',
    });

    state.setPendingAssignment({
      roomId: 'room_incident',
      ticketId: 'INC-1042',
      assigneeId: 'person_priya',
      assigneeName: 'Priya Raman',
      proposedByMessageId: 'msg_initial',
      proposedAt: Date.now(),
    });

    await workflow.handle(
      makeMessageEvent('approve assignment INC-1042', 'msg_approve'),
      client,
    );

    assert.ok(assigned);
    assert.ok(handoffCreated);
    assert.equal(posted.length, 1);
    assert.match(posted[0].content, /Assignment completed: INC-1042 was assigned to Priya Raman/);
    assert.match(posted[0].content, /Handoff HANDOFF-42/);
    assert.match(posted[0].content, /@release-agent/);
    assert.equal(state.pendingAssignment('room_incident'), null);
  } finally {
    cleanup();
  }
});

test('Switch workflow rejects mismatched and expired assignments', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {} as CompanyBrainRepository;

    let nowTime = 1000000;
    const workflow = new CompanyBrainSwitchWorkflow({
      repository,
      state,
      now: () => nowTime,
    });

    state.setPendingAssignment({
      roomId: 'room_incident',
      ticketId: 'INC-1042',
      assigneeId: 'person_priya',
      assigneeName: 'Priya Raman',
      proposedByMessageId: 'msg_initial',
      proposedAt: nowTime,
    });

    // 1. Mismatched ticket ID
    await workflow.handle(makeMessageEvent('approve assignment INC-9999', 'msg_wrong'), client);
    assert.match(posted[0].content, /pending proposal is for INC-1042, not INC-9999/);
    assert.ok(state.pendingAssignment('room_incident')); // still pending

    // 2. Advance time past 15-minute window (15 * 60 * 1000 ms + 1)
    nowTime += 16 * 60 * 1000;
    await workflow.handle(makeMessageEvent('approve assignment INC-1042', 'msg_expired'), client);
    assert.match(posted[1].content, /proposal expired/);
    assert.equal(state.pendingAssignment('room_incident'), null); // cleared
  } finally {
    cleanup();
  }
});

test('Switch workflow supports cancel assignment command', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {} as CompanyBrainRepository;
    const workflow = new CompanyBrainSwitchWorkflow({ repository, state });

    state.setPendingAssignment({
      roomId: 'room_incident',
      ticketId: 'INC-1042',
      assigneeId: 'person_priya',
      assigneeName: 'Priya Raman',
      proposedByMessageId: 'msg_initial',
      proposedAt: Date.now(),
    });

    await workflow.handle(makeMessageEvent('cancel assignment', 'msg_cancel'), client);
    assert.match(posted[0].content, /pending ticket assignment was cancelled/);
    assert.equal(state.pendingAssignment('room_incident'), null);
  } finally {
    cleanup();
  }
});

test('Switch workflow recalls room memory graph', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const mockMemory: CompanyMemory = {
      sessionId: 'switch_room_room_incident',
      questions: [
        {
          questionId: 'q_1',
          question: 'Who owns OAuth?',
          answerId: 'ans_1',
          answer: 'Priya Raman.',
          sourceIds: ['SRC-ADR-014'],
        },
      ],
      actions: [
        {
          actionId: 'ACT-1',
          kind: 'assign_ticket',
          status: 'completed',
          targetId: 'INC-1042',
        },
      ],
      handoffs: [
        {
          handoffId: 'H-1',
          fromAgent: 'aqua-company-brain',
          toAgent: 'release-agent',
          summary: 'INC-1042 assigned to Priya.',
        },
      ],
    };

    const repository = {
      recallMemory: async () => mockMemory,
    } as unknown as CompanyBrainRepository;

    const workflow = new CompanyBrainSwitchWorkflow({ repository, state });

    await workflow.handle(makeMessageEvent('show room memory', 'msg_mem'), client);
    assert.equal(posted.length, 1);
    assert.match(posted[0].content, /Recent grounded answers:/);
    assert.match(posted[0].content, /Who owns OAuth\?/);
    assert.match(posted[0].content, /Actions:/);
    assert.match(posted[0].content, /assign_ticket: INC-1042/);
    assert.match(posted[0].content, /Handoffs:/);
  } finally {
    cleanup();
  }
});

test('Switch workflow strictly refuses to fabricate an answer when evidence is missing', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {
      ask: async () => null, // No connected evidence
    } as unknown as CompanyBrainRepository;

    const workflow = new CompanyBrainSwitchWorkflow({ repository, state });

    await workflow.handle(makeMessageEvent('Who wrote the billing service?', 'msg_ungrounded'), client);
    assert.equal(posted.length, 1);
    assert.match(posted[0].content, /could not find connected, current evidence in FalkorDB, so I will not guess/);
  } finally {
    cleanup();
  }
});

test('Switch workflow fails closed gracefully if repository throws', async () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    const { client, posted } = createMockClient();
    const repository = {
      ask: async () => {
        throw new Error('Database connection refused');
      },
    } as unknown as CompanyBrainRepository;

    const workflow = new CompanyBrainSwitchWorkflow({ repository, state });

    await workflow.handle(makeMessageEvent('Who owns OAuth?', 'msg_error'), client);
    assert.equal(posted.length, 1);
    assert.match(posted[0].content, /Company Brain is unavailable right now. I did not make any assignment or invent an answer./);
  } finally {
    cleanup();
  }
});
