// Typed client for "full access" for the project (/api/v1/permissions/full-access). The dev proxy adds the tokens; changing
// the setting needs the UI token, which only the proxy (and the packaged app's own page) holds.

import { JsonResponseError, readResponseJson, responseErrorMessage } from './jsonResponse.ts';

export type LayerState = 'on' | 'off' | 'unavailable';

export interface FullAccessLayer {
  id: 'tools' | 'files' | 'browser' | 'desktop';
  label: string;
  state: LayerState;
  note: string;
}

export interface FullAccessStatus {
  enabled: boolean;
  since_unix_ms: number | null;
  project_root: string | null;
  layers: FullAccessLayer[];
  /** Things that could not be set up (a missing folder, file tools that are off). Empty when everything took effect. */
  problems: string[];
  runs_without_asking: string[];
  still_asks: string[];
  never_allowed: string[];
}

/** An error whose message is already worded for the person reading it. */
export class PermissionError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

async function json<T>(r: Response): Promise<T> {
  let body: unknown;
  try { body = await readResponseJson(r); }
  catch (error) { throw error instanceof JsonResponseError ? new PermissionError(error.message, r.status) : error; }
  if (r.ok) {
    if (body === null) throw new PermissionError('Aqua sent an empty response. Please try again.', r.status);
    return body as T;
  }
  const detail = responseErrorMessage(body);
  if (r.status === 403) throw new PermissionError('Only the Aqua app can change this. Open it from the Aqua window, not from a script or another program.', 403);
  if (r.status === 409) throw new PermissionError(detail || 'The tool policy is locked, so full access cannot be turned on. Fix data/tools/policy.json first.', 409);
  throw new PermissionError(detail || `Aqua answered ${r.status} ${r.statusText}`, r.status);
}

export const permissionsApi = {
  fullAccess: () => fetch('/api/v1/permissions/full-access').then((r) => json<FullAccessStatus>(r)),
  setFullAccess: (enabled: boolean) =>
    fetch('/api/v1/permissions/full-access', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ enabled }) }).then((r) => json<FullAccessStatus>(r)),
};
