import readline from 'node:readline/promises';
import { stdin as input, stdout as output } from 'node:process';
import { CompanyBrainRepository, type CompanyAnswer, type CompanyMemory } from '../companyBrainRepository.ts';
import { CompanyBrainSwitchWorkflow } from './companyBrainWorkflow.ts';
import type { SwitchClient, SwitchDomainEvent } from './switchClient.ts';
import { SwitchWorkflowState } from './workflowState.ts';

// Simulated company evidence (matches standard seed dataset)
const simulatedAnswer: CompanyAnswer = {
  interactionId: 'q_interactive_1',
  answerId: 'ans_interactive_1',
  sessionId: 'switch_room_incident_triage',
  graphRequired: true,
  intent: 'ownership',
  answer: 'Priya Raman (Staff Engineer, Identity Engineering) owns the Identity Platform and should handle the OAuth production incident. Earlier decision DEC-014 (Use staged PKCE migration) affects the fix.',
  facts: {
    ticketId: 'INC-1042',
    ticketTitle: 'OAuth production incident',
    projectName: 'Identity Platform',
    ownerName: 'Priya Raman',
    ownerRole: 'Staff Engineer',
    teamName: 'Identity Engineering',
    decisionId: 'DEC-014',
    decisionTitle: 'Use staged PKCE migration',
    decisionSummary: 'Roll out PKCE by client cohort and preserve the legacy callback during migration.',
  },
  action: {
    kind: 'assign_ticket',
    ticketId: 'INC-1042',
    assigneeId: 'person_priya',
    label: 'Assign INC-1042 to Priya Raman',
  },
  paths: [
    ['INC-1042', 'BELONGS_TO', 'Identity Platform', 'OWNS', 'Priya Raman', 'MEMBER_OF', 'Identity Engineering'],
    ['DEC-014', 'AFFECTS', 'Identity Platform', 'SUPPORTED_BY', 'SRC-ADR-014'],
  ],
  sources: [
    {
      id: 'SRC-ADR-014',
      title: 'ADR-014: OAuth PKCE migration',
      uri: 'company://docs/ADR-014',
      kind: 'document',
    },
    {
      id: 'SRC-CHAT-221',
      title: 'Identity incident handoff',
      uri: 'company://chat/identity/221',
      kind: 'chat',
    },
  ],
};

async function main() {
  console.log('\n========================================================');
  console.log('  ⚡ Switch Room Simulator: Aqua Company Brain');
  console.log('========================================================');
  console.log('Room: #incident-triage (Mattermost/Slack via Switch)');
  console.log('Teammate: You (Sam / Priya)');
  console.log('Agent:    @aqua-company-brain\n');
  console.log('Suggested prompts to try:');
  console.log('  1. @aqua-company-brain Who should handle the OAuth production incident?');
  console.log('  2. approve assignment INC-1042');
  console.log('  3. show room memory');
  console.log('  Type "exit" to quit.\n');

  const memoryStore: CompanyMemory = {
    sessionId: 'switch_room_incident_triage',
    questions: [],
    actions: [],
    handoffs: [],
  };

  const mockRepo = {
    ask: async (question: string) => {
      if (question.toLowerCase().includes('oauth') || question.toLowerCase().includes('incident') || question.toLowerCase().includes('who')) {
        memoryStore.questions.push({
          questionId: 'q_1',
          question,
          answerId: 'ans_1',
          answer: simulatedAnswer.answer,
          sourceIds: ['SRC-ADR-014', 'SRC-CHAT-221'],
        });
        return simulatedAnswer;
      }
      return null;
    },
    assignTicket: async (ticketId: string, assigneeId: string) => {
      if (ticketId.toUpperCase() === 'INC-1042' && assigneeId === 'person_priya') {
        memoryStore.actions.push({
          actionId: 'ACT-9042',
          kind: 'assign_ticket',
          status: 'completed',
          targetId: ticketId,
        });
        return { actionId: 'ACT-9042' };
      }
      return null;
    },
    createHandoff: async (_tenantId: string, _sessionId: string, summary: string) => {
      memoryStore.handoffs.push({
        handoffId: 'HANDOFF-101',
        fromAgent: 'aqua-company-brain',
        toAgent: 'release-agent',
        summary,
      });
      return 'HANDOFF-101';
    },
    recallMemory: async () => memoryStore,
  } as unknown as CompanyBrainRepository;

  const state = new SwitchWorkflowState('.interactive-switch-workflow.json');
  const workflow = new CompanyBrainSwitchWorkflow({
    repository: mockRepo,
    state,
    agentName: 'aqua-company-brain',
    incidentAgentName: 'release-agent',
  });

  const mockClient = {
    postMessage: async (_roomId: string, content: string) => {
      console.log(`\n🤖 [@aqua-company-brain]:\n${content}\n`);
      return { success: true };
    },
  } as unknown as SwitchClient;

  const rl = readline.createInterface({ input, output });

  try {
    let msgCounter = 1;
    while (true) {
      const userText = await rl.question('💬 [You]: ');
      const trimmed = userText.trim();
      if (!trimmed) continue;
      if (trimmed.toLowerCase() === 'exit' || trimmed.toLowerCase() === 'quit') break;

      const event: SwitchDomainEvent = {
        event: 'message',
        id: msgCounter++,
        data: {
          type: 'message',
          room_id: 'incident_triage',
          bridge_id: 'bridge_sim',
          channel_type: 'mattermost',
          sequence: msgCounter,
          payload: {
            addressed: true,
            sender: 'user_teammate',
            sender_name: 'You',
            message_id: `msg_${Date.now()}_${msgCounter}`,
            body: trimmed,
            timestamp: Date.now(),
            thread_id: null,
            attachments: [],
          },
        },
      };

      await workflow.handle(event, mockClient);
    }
  } finally {
    rl.close();
    try {
      const { rmSync } = await import('node:fs');
      rmSync('.interactive-switch-workflow.json', { force: true });
    } catch {}
  }
}

void main();
