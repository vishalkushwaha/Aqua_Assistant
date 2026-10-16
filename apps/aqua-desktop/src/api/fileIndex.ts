export interface IndexStatus {
  enabled: boolean;
  index_files: boolean;
  /** disabled | starting | scanning | idle | paused | error */
  state: string;
  paused_reason: string | null;
  folders: number;
  programs: number;
  files: number;
  queue: number;
  tier1_ready: boolean;
  complete: boolean;
  volumes: { guid: string; online: boolean }[];
  watching: number;
  polled: number;
  last_update: number;
  cpu_seconds: number;
  db_bytes: number;
  lookup_p50_ms: number;
  lookup_p95_ms: number;
  overruns: number;
  events: number;
  listed: number;
  error: string | null;
  restarts: number;
  rebuilt_after_damage: boolean;
}

export interface IndexSettings {
  enabled: boolean;
  index_documents: boolean;
  index_files: boolean;
  other_drives: boolean;
  extra_roots: string[];
  exclude_paths: string[];
}

export interface IndexState {
  status: IndexStatus;
  settings: IndexSettings;
}

async function post(path: string, body?: unknown): Promise<{ ok: boolean; error?: string }> {
  try {
    const res = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body ?? {}) });
    const data = await res.json().catch(() => ({}));
    return res.ok ? { ok: true } : { ok: false, error: data.error ?? `HTTP ${res.status}` };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export const fileIndexApi = {
  async state(): Promise<IndexState | null> {
    try {
      const res = await fetch('/api/v1/index/status', { headers: { Accept: 'application/json' } });
      return res.ok ? ((await res.json()) as IndexState) : null;
    } catch {
      return null;
    }
  },
  settings: (patch: Partial<IndexSettings>) => post('/api/v1/index/settings', patch),
  pause: (paused: boolean) => post('/api/v1/index/pause', { paused }),
  rebuild: () => post('/api/v1/index/rebuild'),
  forget: () => post('/api/v1/index/forget'),
};
