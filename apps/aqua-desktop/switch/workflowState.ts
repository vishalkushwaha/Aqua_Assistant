import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

export const SWITCH_WORKFLOW_STATE_FILE = resolve(process.cwd(), '.switch-workflow.json');

export interface PendingAssignment {
  roomId: string;
  ticketId: string;
  assigneeId: string;
  assigneeName: string;
  proposedByMessageId: string;
  proposedAt: number;
}

interface WorkflowStateData {
  pendingAssignments: Record<string, PendingAssignment>;
  processedMessageIds: string[];
}

const EMPTY_STATE: WorkflowStateData = { pendingAssignments: {}, processedMessageIds: [] };

export class SwitchWorkflowState {
  private data: WorkflowStateData;
  private readonly filePath: string;

  constructor(filePath = SWITCH_WORKFLOW_STATE_FILE) {
    this.filePath = filePath;
    this.data = this.load();
  }

  wasProcessed(messageId: string): boolean {
    return this.data.processedMessageIds.includes(messageId);
  }

  markProcessed(messageId: string): void {
    this.data.processedMessageIds.push(messageId);
    this.data.processedMessageIds = this.data.processedMessageIds.slice(-500);
    this.save();
  }

  pendingAssignment(roomId: string): PendingAssignment | null {
    return this.data.pendingAssignments[roomId] || null;
  }

  setPendingAssignment(assignment: PendingAssignment): void {
    this.data.pendingAssignments[assignment.roomId] = assignment;
    this.save();
  }

  clearPendingAssignment(roomId: string): void {
    delete this.data.pendingAssignments[roomId];
    this.save();
  }

  private load(): WorkflowStateData {
    if (!existsSync(this.filePath)) return { ...EMPTY_STATE, pendingAssignments: {}, processedMessageIds: [] };
    try {
      const parsed = JSON.parse(readFileSync(this.filePath, 'utf8')) as Partial<WorkflowStateData>;
      return {
        pendingAssignments: parsed.pendingAssignments && typeof parsed.pendingAssignments === 'object' ? parsed.pendingAssignments : {},
        processedMessageIds: Array.isArray(parsed.processedMessageIds)
          ? parsed.processedMessageIds.filter((id): id is string => typeof id === 'string').slice(-500)
          : [],
      };
    } catch {
      return { ...EMPTY_STATE, pendingAssignments: {}, processedMessageIds: [] };
    }
  }

  private save(): void {
    writeFileSync(this.filePath, `${JSON.stringify(this.data, null, 2)}\n`, 'utf8');
  }
}
