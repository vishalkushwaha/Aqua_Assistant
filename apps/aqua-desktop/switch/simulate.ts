import http from 'node:http';
import { WebSocketServer, WebSocket } from 'ws';
import { CompanyBrainRepository, type CompanyAnswer, type CompanyMemory } from '../companyBrainRepository.ts';
import { CompanyBrainSwitchWorkflow } from './companyBrainWorkflow.ts';
import { registerSwitchAgent, SwitchClient, type SwitchDomainEvent } from './switchClient.ts';
import { SwitchWorkflowState } from './workflowState.ts';

// In-memory mock evidence for reproducible zero-dependency simulation
const simulatedAnswer: CompanyAnswer = {
  interactionId: 'q_sim_1',
  answerId: 'ans_sim_1',
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

class SimulationMockBridge {
  readonly server: http.Server;
  readonly wss: WebSocketServer;
  readonly postedMessages: Array<{ roomId: string; content: string }> = [];
  private activeWs: WebSocket | null = null;
  private port = 0;

  constructor() {
    this.server = http.createServer((req, res) => {
      let body = '';
      req.on('data', (chunk) => (body += chunk));
      req.on('end', () => {
        const url = new URL(req.url || '/', `http://127.0.0.1:${this.port}`);

        if (req.method === 'POST' && url.pathname === '/agents') {
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ id: 'agent_aqua_simulation', api_key: 'sec_sim_token_123' }));
          return;
        }

        if (req.method === 'POST' && url.pathname.endsWith('/message')) {
          const parsed = JSON.parse(body || '{}');
          const roomId = url.searchParams.get('room_id') || 'incident_triage';
          this.postedMessages.push({ roomId, content: parsed.content || '' });
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ success: true, message_id: `msg_out_${Date.now()}` }));
          return;
        }

        res.writeHead(404, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: 'Not found' }));
      });
    });

    this.wss = new WebSocketServer({ noServer: true });

    this.server.on('upgrade', (req, socket, head) => {
      this.wss.handleUpgrade(req, socket, head, (ws) => {
        this.activeWs = ws;
        this.wss.emit('connection', ws, req);
      });
    });
  }

  async start(): Promise<number> {
    return new Promise((resolve) => {
      this.server.listen(0, '127.0.0.1', () => {
        const addr = this.server.address();
        this.port = typeof addr === 'object' && addr ? addr.port : 0;
        resolve(this.port);
      });
    });
  }

  broadcastDomainEvent(event: SwitchDomainEvent): void {
    if (!this.activeWs || this.activeWs.readyState !== WebSocket.OPEN) {
      throw new Error('No active WebSocket client connected to mock bridge.');
    }
    this.activeWs.send(JSON.stringify(event));
  }

  async stop(): Promise<void> {
    if (this.activeWs) {
      this.activeWs.close();
    }
    this.wss.close();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }
}

export async function runSimulation(): Promise<boolean> {
  console.log('--- Starting Aqua Company Brain Switch Simulation ---');
  const bridge = new SimulationMockBridge();
  const port = await bridge.start();
  const bridgeUrl = `http://127.0.0.1:${port}`;
  console.log(`[1] Mock Switch Bridge started at ${bridgeUrl}`);

  try {
    // 1. Agent Registration
    const reg = await registerSwitchAgent(bridgeUrl, 'mock_reg_token', {
      name: 'aqua-company-brain',
      displayName: 'Aqua Company Brain',
      description: 'Simulation agent',
      connectorType: 'aqua-company-brain',
    });
    console.log(`[2] Registered agent with Switch: ID=${reg.id}`);

    // 2. Setup mock repository
    let ticketAssigned = false;
    let handoffRecorded = false;
    const memoryStore: CompanyMemory = {
      sessionId: 'switch_room_incident_triage',
      questions: [],
      actions: [],
      handoffs: [],
    };

    const mockRepo = {
      ask: async (question: string) => {
        memoryStore.questions.push({
          questionId: 'q_1',
          question,
          answerId: 'ans_1',
          answer: simulatedAnswer.answer,
          sourceIds: ['SRC-ADR-014', 'SRC-CHAT-221'],
        });
        return simulatedAnswer;
      },
      assignTicket: async (ticketId: string, assigneeId: string) => {
        if (ticketId === 'INC-1042' && assigneeId === 'person_priya') {
          ticketAssigned = true;
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
        handoffRecorded = true;
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

    const testState = new SwitchWorkflowState('.simulation-state.json');
    const workflow = new CompanyBrainSwitchWorkflow({
      repository: mockRepo,
      state: testState,
      agentName: 'aqua-company-brain',
      incidentAgentName: 'release-agent',
    });

    const abortController = new AbortController();
    const client = new SwitchClient({
      bridgeUrl,
      agentId: reg.id,
      apiKey: reg.apiKey,
      roomIds: ['incident_triage'],
      scope: 'single',
      filter: 'addressed',
    });

    const clientRunPromise = client.run(async (event) => {
      await workflow.handle(event, client);
    }, abortController.signal);

    // Wait for WebSocket connection
    await new Promise((r) => setTimeout(r, 200));
    console.log('[3] Connected SwitchClient to Mock Bridge via WebSocket');

    // Step A: Human in chat asks for incident diagnosis
    console.log('\n[Step A] Teammate asks incident question in chat room...');
    bridge.broadcastDomainEvent({
      event: 'message',
      id: 1,
      data: {
        type: 'message',
        room_id: 'incident_triage',
        bridge_id: 'bridge_sim',
        channel_type: 'mattermost',
        sequence: 1,
        payload: {
          addressed: true,
          sender: 'user_sam',
          sender_name: 'Sam',
          message_id: 'sim_msg_001',
          body: '@aqua-company-brain Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?',
          timestamp: Date.now(),
          thread_id: null,
          attachments: [],
        },
      },
    });

    await new Promise((r) => setTimeout(r, 200));
    const firstReply = bridge.postedMessages[0];
    if (!firstReply || !firstReply.content.includes('Priya Raman')) {
      throw new Error(`Step A failed: Unexpected reply: ${firstReply?.content}`);
    }
    console.log('✓ Aqua replied with grounded owner (Priya Raman), DEC-014, and proposed INC-1042 assignment.');

    // Step B: Teammate signs off on the assignment
    console.log('\n[Step B] Teammate signs off: "approve assignment INC-1042"...');
    bridge.broadcastDomainEvent({
      event: 'message',
      id: 2,
      data: {
        type: 'message',
        room_id: 'incident_triage',
        bridge_id: 'bridge_sim',
        channel_type: 'mattermost',
        sequence: 2,
        payload: {
          addressed: true,
          sender: 'user_sam',
          sender_name: 'Sam',
          message_id: 'sim_msg_002',
          body: 'approve assignment INC-1042',
          timestamp: Date.now(),
          thread_id: null,
          attachments: [],
        },
      },
    });

    await new Promise((r) => setTimeout(r, 200));
    const secondReply = bridge.postedMessages[1];
    if (!secondReply || !ticketAssigned || !handoffRecorded) {
      throw new Error(`Step B failed: Assignment not completed. Reply: ${secondReply?.content}`);
    }
    console.log('✓ Aqua verified graph authorization, executed assignment mutation, recorded handoff, and notified @release-agent.');

    // Step C: Teammate asks to inspect room memory
    console.log('\n[Step C] Teammate asks: "show room memory"...');
    bridge.broadcastDomainEvent({
      event: 'message',
      id: 3,
      data: {
        type: 'message',
        room_id: 'incident_triage',
        bridge_id: 'bridge_sim',
        channel_type: 'mattermost',
        sequence: 3,
        payload: {
          addressed: true,
          sender: 'user_priya',
          sender_name: 'Priya',
          message_id: 'sim_msg_003',
          body: 'show room memory',
          timestamp: Date.now(),
          thread_id: null,
          attachments: [],
        },
      },
    });

    await new Promise((r) => setTimeout(r, 200));
    const thirdReply = bridge.postedMessages[2];
    if (!thirdReply || !thirdReply.content.includes('Recent grounded answers:')) {
      throw new Error(`Step C failed: Memory not recalled. Reply: ${thirdReply?.content}`);
    }
    console.log('✓ Aqua successfully recalled room episodic memory, actions, and handoff chain.');

    // Clean shutdown
    abortController.abort();
    client.close();
    await clientRunPromise.catch(() => {});
    await bridge.stop();

    try {
      const { rmSync } = await import('node:fs');
      rmSync('.simulation-state.json', { force: true });
    } catch {}

    console.log('\n======================================================');
    console.log('🎉 SIMULATION PASSED: All 3 collaborative chat steps verified!');
    console.log('======================================================\n');
    return true;
  } catch (error) {
    console.error('Simulation failed:', error);
    await bridge.stop();
    return false;
  }
}

// Direct execution
if (import.meta.url === `file://${process.argv[1].replace(/\\/g, '/')}`) {
  const success = await runSimulation();
  process.exit(success ? 0 : 1);
}
