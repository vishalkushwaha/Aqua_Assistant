import {
  DEFAULT_COMPANY_TENANT_ID,
  type CompanyAnswer,
  type CompanyBrainRepository,
  type CompanyMemory,
} from '../companyBrainRepository.ts';
import { type SwitchClient, type SwitchDomainEvent, type SwitchMessagePayload } from './switchClient.ts';
import { SwitchWorkflowState } from './workflowState.ts';

const APPROVAL_WINDOW_MS = 15 * 60 * 1000;

export interface CompanyBrainWorkflowOptions {
  repository: CompanyBrainRepository;
  state?: SwitchWorkflowState;
  tenantId?: string;
  agentName?: string;
  incidentAgentName?: string;
  now?: () => number;
}

function isMessagePayload(payload: Record<string, unknown> | SwitchMessagePayload): payload is SwitchMessagePayload {
  return typeof payload.message_id === 'string'
    && typeof payload.body === 'string'
    && typeof payload.sender === 'string'
    && payload.addressed === true;
}

function sessionId(roomId: string): string {
  return `switch_room_${roomId}`;
}

function compactLine(value: string): string {
  return value.replace(/\s+/g, ' ').trim();
}

export class CompanyBrainSwitchWorkflow {
  private readonly repository: CompanyBrainRepository;
  private readonly state: SwitchWorkflowState;
  private readonly tenantId: string;
  private readonly agentName: string;
  private readonly incidentAgentName: string;
  private readonly now: () => number;

  constructor(options: CompanyBrainWorkflowOptions) {
    this.repository = options.repository;
    this.state = options.state || new SwitchWorkflowState();
    this.tenantId = options.tenantId || DEFAULT_COMPANY_TENANT_ID;
    this.agentName = options.agentName || 'aqua-company-brain';
    this.incidentAgentName = options.incidentAgentName || '';
    this.now = options.now || Date.now;
  }

  async handle(event: SwitchDomainEvent, client: SwitchClient): Promise<void> {
    if (event.event !== 'message' || !isMessagePayload(event.data.payload)) return;
    const payload = event.data.payload;
    if (this.state.wasProcessed(payload.message_id)) return;

    const message = this.withoutLeadingMention(payload.body);
    if (!message) {
      await client.postMessage(event.data.room_id, 'Ask me who owns an incident, why a decision was made, or what project context applies.');
      this.state.markProcessed(payload.message_id);
      return;
    }

    try {
      if (await this.handleApproval(message, event.data.room_id, client)) {
        this.state.markProcessed(payload.message_id);
        return;
      }
      if (this.isCancel(message)) {
        this.state.clearPendingAssignment(event.data.room_id);
        await client.postMessage(event.data.room_id, 'The pending ticket assignment was cancelled.');
        this.state.markProcessed(payload.message_id);
        return;
      }
      if (this.isMemoryRequest(message)) {
        const memory = await this.repository.recallMemory(this.tenantId, sessionId(event.data.room_id));
        await client.postMessage(event.data.room_id, this.formatMemory(memory));
        this.state.markProcessed(payload.message_id);
        return;
      }

      const answer = await this.repository.ask(message, this.tenantId, sessionId(event.data.room_id));
      if (!answer) {
        await client.postMessage(event.data.room_id, 'I could not find connected, current evidence in FalkorDB, so I will not guess.');
        this.state.markProcessed(payload.message_id);
        return;
      }

      if (answer.action) {
        this.state.setPendingAssignment({
          roomId: event.data.room_id,
          ticketId: answer.action.ticketId,
          assigneeId: answer.action.assigneeId,
          assigneeName: answer.facts.ownerName || answer.action.assigneeId,
          proposedByMessageId: payload.message_id,
          proposedAt: this.now(),
        });
      }
      await client.postMessage(event.data.room_id, this.formatAnswer(answer));
      this.state.markProcessed(payload.message_id);
    } catch (error) {
      console.error('[switch-company-brain] Could not handle room message:', error);
      await client.postMessage(event.data.room_id, 'Company Brain is unavailable right now. I did not make any assignment or invent an answer.');
      this.state.markProcessed(payload.message_id);
    }
  }

  private withoutLeadingMention(body: string): string {
    const escapedName = this.agentName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    return body.trim().replace(new RegExp(`^@${escapedName}[,:]?\\s*`, 'i'), '').trim();
  }

  private async handleApproval(message: string, roomId: string, client: SwitchClient): Promise<boolean> {
    const match = message.match(/^\/?approve\s+assignment(?:\s+([^\s]+))?\s*$/i);
    if (!match) return false;
    const pending = this.state.pendingAssignment(roomId);
    if (!pending) {
      await client.postMessage(roomId, 'There is no pending graph-validated assignment to approve.');
      return true;
    }
    if (this.now() - pending.proposedAt > APPROVAL_WINDOW_MS) {
      this.state.clearPendingAssignment(roomId);
      await client.postMessage(roomId, 'That assignment proposal expired. Ask the Company Brain again to refresh its graph evidence.');
      return true;
    }
    if (match[1] && match[1].toLocaleLowerCase() !== pending.ticketId.toLocaleLowerCase()) {
      await client.postMessage(roomId, `The pending proposal is for ${pending.ticketId}, not ${match[1]}.`);
      return true;
    }

    const result = await this.repository.assignTicket(
      pending.ticketId,
      pending.assigneeId,
      this.tenantId,
      sessionId(roomId),
    );
    if (!result) {
      this.state.clearPendingAssignment(roomId);
      await client.postMessage(roomId, 'FalkorDB no longer validates that ownership path, so the assignment was not made.');
      return true;
    }

    const summary = `${pending.ticketId} was assigned to ${pending.assigneeName} after explicit approval in Switch. Action ${result.actionId}.`;
    const handoffId = await this.repository.createHandoff(this.tenantId, sessionId(roomId), summary);
    this.state.clearPendingAssignment(roomId);
    const mention = this.incidentAgentName ? ` @${this.incidentAgentName}` : '';
    await client.postMessage(roomId, `Assignment completed: ${summary} Handoff ${handoffId} is stored in the graph.${mention}`);
    return true;
  }

  private isCancel(message: string): boolean {
    return /^\/?cancel\s+assignment\s*$/i.test(message);
  }

  private isMemoryRequest(message: string): boolean {
    return /^(show\s+)?(room\s+)?memory\s*$/i.test(message);
  }

  private formatAnswer(answer: CompanyAnswer): string {
    const sources = answer.sources.length > 0
      ? answer.sources.map((source) => `- ${source.id}: ${source.title} (${source.uri})`).join('\n')
      : '- No source records returned';
    const paths = answer.paths.map((path) => `- ${path.filter(Boolean).join(' → ')}`).join('\n');
    const proposal = answer.action
      ? `\n\nProposed action: ${answer.action.label}. Reply \`approve assignment ${answer.action.ticketId}\` within 15 minutes to execute it, or \`cancel assignment\`.`
      : '';
    return `${compactLine(answer.answer)}\n\nEvidence paths:\n${paths}\n\nSources:\n${sources}${proposal}`.slice(0, 12_000);
  }

  private formatMemory(memory: CompanyMemory): string {
    if (memory.questions.length === 0 && memory.actions.length === 0 && memory.handoffs.length === 0) {
      return 'This Switch room has no Company Brain memory yet.';
    }
    const questions = memory.questions.slice(0, 5).map((item) => `- Q: ${compactLine(String(item.question || ''))}\n  A: ${compactLine(String(item.answer || ''))}`);
    const actions = memory.actions.slice(0, 5).map((item) => `- ${item.kind}: ${item.targetId} (${item.status})`);
    const handoffs = memory.handoffs.slice(0, 5).map((item) => `- ${item.fromAgent} → ${item.toAgent}: ${compactLine(String(item.summary || ''))}`);
    return [
      questions.length ? `Recent grounded answers:\n${questions.join('\n')}` : '',
      actions.length ? `Actions:\n${actions.join('\n')}` : '',
      handoffs.length ? `Handoffs:\n${handoffs.join('\n')}` : '',
    ].filter(Boolean).join('\n\n').slice(0, 12_000);
  }
}
