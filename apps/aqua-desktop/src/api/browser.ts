// Typed client for aqua-core's policy-guarded browser API (/api/v1/browser/*).
// The dev proxy adds the API token, so these are plain relative fetches.

export type BackendKind = 'managed' | 'extension';
export type SiteMode = 'ask' | 'allowlist' | 'denylist';
export type JavascriptPolicy = 'off' | 'allowed_sites';
export type ProfileMode = 'persistent' | 'ephemeral';
export type BrowserChannel = 'msedge' | 'chrome';
export type Decision = 'allow' | 'allow_always' | 'deny';

export interface BrowserStatus {
  backend: BackendKind;
  installed: boolean;
  running: boolean;
  initialized: boolean;
  tool_count: number;
  tabs: number;
  url: string | null;
  browser: string;
  profile: ProfileMode;
  headless: boolean;
  pending_confirmations: number;
  hint: string;
}

export interface BrowserPolicy {
  mode: SiteMode;
  allow: string[];
  deny: string[];
  javascript: JavascriptPolicy;
  javascript_sites: string[];
  confirm_irreversible: boolean;
  confirm_timeout_secs: number;
  profile: ProfileMode;
  browser: BrowserChannel;
  headless: boolean;
  max_tabs: number;
  idle_shutdown_minutes: number;
  snapshot_budget_chars: number;
}

export interface ConfirmRequest {
  request_id: string;
  kind: 'site' | 'action' | 'tool';
  tool: string;
  reason: string;
  url: string | null;
  site: string | null;
  created_unix_ms: number;
  expires_in_secs: number;
  /** Tool approvals: this answer is never remembered, so "for this session" is not offered. */
  always_ask?: boolean;
  /** Tool approvals: what the call can do (WRITE, EXECUTE, NETWORK…). */
  classes?: string[];
}

export interface BrowserObservation {
  ok: boolean;
  tool: string;
  backend: string;
  url: string | null;
  title: string | null;
  text: string;
  data?: unknown;
  truncated: boolean;
  artifacts: { kind: string; path: string }[];
  error: { code: string; message: string; retryable: boolean } | null;
  duration_ms: number;
}

async function json<T>(res: Response): Promise<T> {
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error((body as { error?: string }).error || `Request failed (${res.status})`);
  }
  return body as T;
}

const send = (method: string, body: unknown) => ({
  method,
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify(body),
});

export const browserApi = {
  status: () => fetch('/api/v1/browser/status').then((r) => json<BrowserStatus>(r)),
  start: () => fetch('/api/v1/browser/start', { method: 'POST' }).then((r) => json<BrowserStatus>(r)),
  stop: () => fetch('/api/v1/browser/stop', { method: 'POST' }).then((r) => json<BrowserStatus>(r)),
  setBackend: (backend: BackendKind) =>
    fetch('/api/v1/browser/backend', send('PUT', { backend })).then((r) => json<BrowserStatus>(r)),
  policy: () => fetch('/api/v1/browser/policy').then((r) => json<BrowserPolicy>(r)),
  savePolicy: (p: BrowserPolicy) =>
    fetch('/api/v1/browser/policy', send('PUT', p)).then((r) => json<BrowserPolicy>(r)),
  resetProfile: () =>
    fetch('/api/v1/browser/session', send('POST', { action: 'reset_profile' })).then((r) => json<{ ok: boolean }>(r)),
  confirmations: () =>
    fetch('/api/v1/browser/confirmations').then((r) => json<{ pending: ConfirmRequest[] }>(r)),
  confirm: (request_id: string, decision: Decision) =>
    fetch('/api/v1/browser/confirm', send('POST', { request_id, decision })).then((r) => json<{ ok: boolean }>(r)),
  call: (tool: string, args: Record<string, unknown> = {}) =>
    fetch('/api/v1/browser/call', send('POST', { tool, arguments: args })).then((r) => json<BrowserObservation>(r)),
  screenshotUrl: (path: string) => `/api/v1/browser/screenshot?path=${encodeURIComponent(path)}`,
};
