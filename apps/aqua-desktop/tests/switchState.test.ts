import assert from 'node:assert/strict';
import { rmSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import { SwitchWorkflowState } from '../switch/workflowState.ts';

const TEST_STATE_FILE = resolve(process.cwd(), '.test-switch-workflow.json');

function cleanup() {
  try {
    rmSync(TEST_STATE_FILE, { force: true });
  } catch {
    // ignore
  }
}

test('SwitchWorkflowState tracks processed messages and deduplicates', () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    assert.equal(state.wasProcessed('msg_101'), false);

    state.markProcessed('msg_101');
    assert.equal(state.wasProcessed('msg_101'), true);
    assert.equal(state.wasProcessed('msg_102'), false);

    // Persists across new instances
    const reloaded = new SwitchWorkflowState(TEST_STATE_FILE);
    assert.equal(reloaded.wasProcessed('msg_101'), true);
  } finally {
    cleanup();
  }
});

test('SwitchWorkflowState manages pending room assignments', () => {
  cleanup();
  try {
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    assert.equal(state.pendingAssignment('room_incident'), null);

    state.setPendingAssignment({
      roomId: 'room_incident',
      ticketId: 'INC-1042',
      assigneeId: 'person_priya',
      assigneeName: 'Priya Raman',
      proposedByMessageId: 'msg_201',
      proposedAt: 1760000000000,
    });

    const pending = state.pendingAssignment('room_incident');
    assert.ok(pending);
    assert.equal(pending.ticketId, 'INC-1042');
    assert.equal(pending.assigneeName, 'Priya Raman');

    state.clearPendingAssignment('room_incident');
    assert.equal(state.pendingAssignment('room_incident'), null);
  } finally {
    cleanup();
  }
});

test('SwitchWorkflowState safely recovers from corrupted state file', () => {
  cleanup();
  try {
    writeFileSync(TEST_STATE_FILE, '{ invalid json syntax !!!', 'utf8');
    const state = new SwitchWorkflowState(TEST_STATE_FILE);
    assert.equal(state.wasProcessed('msg_any'), false);
    assert.equal(state.pendingAssignment('room_any'), null);
  } finally {
    cleanup();
  }
});
