// Typed client for aqua-core's workflow automation (/api/v1/workflows/*). The dev proxy adds the
// tokens. Reading needs any token; creating, editing, enabling, running, cancelling and deleting need
// the UI token, which the proxy sends.

export type Trigger =
  | { type: 'manual' }
  | { type: 'schedule'; cron: string }
  | { type: 'interval'; every_secs: number }
  | { type: 'startup' }
  | { type: 'webhook' };

export type OnFailure = 'stop' | 'continue';

interface StepBase {
  id: string;
  name: string;
  only_if?: string | null;
  retries: number;
  retry_delay_secs: number;
  on_failure: OnFailure;
  timeout_secs: number;
}

export type Step =
  | (StepBase & { type: 'tool'; tool: string; arguments: Record<string, unknown> })
  | (StepBase & { type: 'agent'; goal: string; max_steps: number })
  | (StepBase & { type: 'set'; value: string });

export interface WorkflowInput {
  name: string;
  description: string;
  enabled: boolean;
  trigger: Trigger;
  steps: Step[];
  catch_up: boolean;
}

export interface Workflow extends WorkflowInput {
  id: number;
  owner_id: string;
  created_at: number;
  updated_at: number;
  next_run_at: number | null;
  last_run_at: number;
  last_status: string;
}

export type RunStatus = 'running' | 'succeeded' | 'partial' | 'failed' | 'cancelled' | 'interrupted' | 'skipped';

export interface StepRun {
  idx: number;
  step_id: string;
  name: string;
  status: 'pending' | 'running' | 'succeeded' | 'failed' | 'skipped';
  attempts: number;
  started_at: number;
  finished_at: number;
  output: string;
  error: string;
}

export interface Run {
  id: number;
  workflow_id: number;
  trigger_kind: string;
  status: RunStatus;
  started_at: number;
  finished_at: number;
  error: string;
  steps: StepRun[];
}

export interface SchedulerStatus {
  running: boolean;
  active_runs: number;
  max_concurrent: number;
  checked_at: number;
}

async function json<T>(r: Response): Promise<T> {
  if (!r.ok) {
    let detail = '';
    try {
      const body = await r.json();
      detail = body?.error ? String(body.error) : '';
    } catch {
      /* not JSON */
    }
    throw new Error(detail || `${r.status} ${r.statusText}`);
  }
  return (await r.json()) as T;
}

const send = (method: string, url: string, body?: unknown) =>
  fetch(url, { method, headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });

export const workflowsApi = {
  list: () => fetch('/api/v1/workflows').then((r) => json<{ workflows: Workflow[]; scheduler: SchedulerStatus | null }>(r)),
  create: (w: WorkflowInput) => send('POST', '/api/v1/workflows', w).then((r) => json<Workflow>(r)),
  update: (id: number, w: WorkflowInput) => send('PUT', `/api/v1/workflows/${id}`, w).then((r) => json<Workflow>(r)),
  remove: (id: number) => send('POST', `/api/v1/workflows/${id}/delete`).then((r) => json<{ deleted: boolean }>(r)),
  enable: (id: number, enabled: boolean) => send('POST', `/api/v1/workflows/${id}/enable`, { enabled }).then((r) => json<Workflow>(r)),
  run: (id: number) => send('POST', `/api/v1/workflows/${id}/run`).then((r) => json<{ run_id: number }>(r)),
  runs: (id: number, limit = 30) => fetch(`/api/v1/workflows/${id}/runs?limit=${limit}`).then((r) => json<{ runs: Run[] }>(r)),
  getRun: (runId: number) => fetch(`/api/v1/workflows/runs/${runId}`).then((r) => json<Run>(r)),
  cancelRun: (runId: number) => send('POST', `/api/v1/workflows/runs/${runId}/cancel`).then((r) => json<{ cancelled: boolean }>(r)),
  previewSchedule: (cron: string, count = 3) => send('POST', '/api/v1/workflows/schedule/preview', { cron, count }).then((r) => json<{ runs: number[] }>(r)),
};

/** A short, human description of when a workflow runs. */
export function describeTrigger(t: Trigger): string {
  switch (t.type) {
    case 'manual':
      return 'Only when you press Run';
    case 'startup':
      return 'Each time Aqua starts';
    case 'webhook':
      return 'When called by a webhook';
    case 'interval': {
      const m = Math.round(t.every_secs / 60);
      return m % 60 === 0 ? `Every ${m / 60} hour${m === 60 ? '' : 's'}` : `Every ${m} minute${m === 1 ? '' : 's'}`;
    }
    case 'schedule': {
      const daily = /^(\d{1,2}) (\d{1,2}) \* \* (\*|1-5)$/.exec(t.cron.trim());
      if (daily) {
        const time = `${daily[2].padStart(2, '0')}:${daily[1].padStart(2, '0')}`;
        return daily[3] === '*' ? `Every day at ${time}` : `Weekdays at ${time}`;
      }
      return `Schedule: ${t.cron}`;
    }
  }
}
