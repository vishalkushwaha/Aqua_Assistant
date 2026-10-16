// Typed client for aqua-core's tool host (/api/v1/tools/*). The dev proxy adds the tokens.
// Reading is open to any token; saving the policy needs the UI token, which the proxy sends.

export type Mode = 'locked' | 'standard' | 'developer';
export type Decision = 'auto' | 'ask' | 'always_ask' | 'deny';

export interface Limits {
  reads_per_minute: number;
  changes_per_minute: number;
  max_concurrent: number;
}

export interface PolicyFile {
  mode: Mode;
  limits: Limits;
  hard_deny: string[];
  tool_overrides: Record<string, Decision>;
}

export interface ToolStatus {
  mode: Mode;
  tool_count: number;
  unavailable: { tool: string; why: string }[];
  audit_path: string;
}

export interface ToolInfo {
  name: string;
  signature: string;
  description: string;
  classes: string[];
  default_decision: Decision;
}

export interface AuditRecord {
  seq: number;
  ts_unix_ms: number;
  request_id: string;
  agent_run_id: string;
  caller: string;
  tool: string;
  args: unknown;
  decision: string;
  classes: string[];
  ok: boolean;
  error_code: string | null;
  duration_ms: number;
}

async function json<T>(r: Response): Promise<T> {
  if (!r.ok) {
    let detail = '';
    try {
      const body = await r.json();
      detail = body?.error ? `: ${body.error}` : '';
    } catch {
      /* not JSON */
    }
    throw new Error(`${r.status} ${r.statusText}${detail}`);
  }
  return (await r.json()) as T;
}

export const toolsApi = {
  status: () => fetch('/api/v1/tools/status').then((r) => json<ToolStatus>(r)),
  list: () => fetch('/api/v1/tools').then((r) => json<{ count: number; tools: ToolInfo[] }>(r)),
  policy: () => fetch('/api/v1/tools/policy').then((r) => json<PolicyFile>(r)),
  savePolicy: (p: PolicyFile) =>
    fetch('/api/v1/tools/policy', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(p) }).then((r) => json<PolicyFile>(r)),
  recent: (limit = 50) => fetch(`/api/v1/tools/audit/recent?limit=${limit}`).then((r) => json<{ records: AuditRecord[] }>(r)),
  verify: () => fetch('/api/v1/tools/audit/verify').then((r) => json<{ ok: boolean; records?: number; note?: string; error?: string }>(r)),
  cancelAgent: () => fetch('/api/v1/agent/cancel', { method: 'POST' }).then((r) => json<{ cancelled: boolean }>(r)),
};
