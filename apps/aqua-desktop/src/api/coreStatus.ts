export interface ModelsInventory {
  deepfilternet_onnx: boolean;
  whisper_ggml: boolean;
  llm_gguf: boolean;
  kokoro_onnx: boolean;
  biometrics_onnx: boolean;
  laya_checkpoints?: boolean;
}

export interface CoreStatusResponse {
  status: string;
  version: string;
  ram_sla_mb: number;
  ram_usage_mb?: number | null;
  voice_standby_active: boolean;
  database_ready: boolean;
  biometric_enrolled: boolean;
  startup_state?: string;
  /** Only in the answer of a core that is still starting: how long it has been starting, in milliseconds. */
  starting_for_ms?: number;
  voice_security?: { enabled: boolean; ready: boolean; note: string };
  noise_engine_ready?: boolean;
  whisper_engine_ready?: boolean;
  llm_engine_ready?: boolean;
  tts_engine_ready?: boolean;
  pipeline_ready?: boolean;
  text_pipeline_ready?: boolean;
  models: ModelsInventory;
}

const STATUS_PATH = '/api/v1/status';

/**
 * The `status` word of the core's answer in its first seconds. With `AQUA_BIND_FIRST=on` the core takes its port at once and
 * answers `/api/v1/status` (and `/api/v1/health`) with HTTP 503 and a body that says `"status": "STARTING"` until it is ready
 * (core/src/startup_health/early.rs). 503 on purpose: a client that waits for a 200 goes on waiting. The body is what tells
 * "starting" from "not there".
 */
export const STARTING_STATUS = 'STARTING';

/** The three things the page can know about the core: it is ready, it answers but is still starting, or it is not there. */
export type CorePhase = 'ready' | 'starting' | 'offline';

/** What one request for the status came to. */
export type StatusReply =
  | { kind: 'status'; status: CoreStatusResponse }
  | { kind: 'starting'; forMs: number | null }
  | { kind: 'unreachable' };

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function millis(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : null;
}

/**
 * Reads one answer. A 2xx answer with a JSON object is the status, as before. A 503 whose body says `STARTING` is a core that
 * is there and not ready. Anything else (a refused connection is handled by the caller, any other not-ok answer, a 503 with
 * some other body such as "Aqua is busy", a body that is not a JSON object) is "not reachable", as before.
 */
export function readStatusReply(httpStatus: number, body: unknown): StatusReply {
  if (!isObject(body)) return { kind: 'unreachable' };
  if (httpStatus >= 200 && httpStatus < 300) return { kind: 'status', status: body as unknown as CoreStatusResponse };
  if (httpStatus === 503 && body.status === STARTING_STATUS) return { kind: 'starting', forMs: millis(body.starting_for_ms) };
  return { kind: 'unreachable' };
}

/**
 * The status of a core that is starting: the word `STARTING`, nothing ready, no model reported. It is a status like any other,
 * so the poller treats it as an answer that is "not up" and asks again every second, the pace it already used for a core
 * that was not up yet. Nothing shows these empty fields: `useCoreStatus` hands components `null` while the core starts.
 */
export function startingStatus(forMs: number | null): CoreStatusResponse {
  return {
    status: STARTING_STATUS,
    startup_state: STARTING_STATUS,
    version: '',
    ram_sla_mb: 0,
    voice_standby_active: false,
    database_ready: false,
    biometric_enrolled: false,
    noise_engine_ready: false,
    whisper_engine_ready: false,
    llm_engine_ready: false,
    tts_engine_ready: false,
    pipeline_ready: false,
    text_pipeline_ready: false,
    models: { deepfilternet_onnx: false, whisper_ggml: false, llm_gguf: false, kokoro_onnx: false, biometrics_onnx: false },
    ...(forMs === null ? {} : { starting_for_ms: forMs }),
  };
}

/** The status the poller keeps for a reply: the status itself, the status of a starting core, or none. */
export function statusFromReply(reply: StatusReply): CoreStatusResponse | null {
  if (reply.kind === 'status') return reply.status;
  if (reply.kind === 'starting') return startingStatus(reply.forMs);
  return null;
}

export async function fetchCoreStatus(doFetch: typeof fetch = (input, init) => fetch(input, init)): Promise<CoreStatusResponse | null> {
  try {
    const res = await doFetch(STATUS_PATH, { headers: { Accept: 'application/json' } });
    let body: unknown = null;
    try {
      body = await res.json();
    } catch {
      // Not JSON: it is read as "not reachable" below.
    }
    return statusFromReply(readStatusReply(res.status, body));
  } catch {
    return null;
  }
}

export function coreConnected(status: CoreStatusResponse | null): boolean {
  return status?.status === 'RUNNING' && status.database_ready === true;
}

/** Ready, starting, or offline. Offline is everything else: not reachable, and also the old case of an answer that is not ready. */
export function corePhase(status: CoreStatusResponse | null): CorePhase {
  if (status?.status === STARTING_STATUS) return 'starting';
  return coreConnected(status) ? 'ready' : 'offline';
}

/** Whole seconds the core says it has been starting, or null when it does not say. */
export function startingSeconds(status: CoreStatusResponse | null): number | null {
  if (status?.status !== STARTING_STATUS) return null;
  const ms = millis(status.starting_for_ms);
  return ms === null ? null : Math.floor(ms / 1000);
}

/** "7 s", or "1 min 5 s" from a minute on. */
export function secondsText(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`;
}

/** What to say while the core starts (`null` when it is not starting): a sentence, and a shorter text for a small label. */
export function describeStarting(status: CoreStatusResponse | null): { seconds: number | null; text: string; short: string } | null {
  if (corePhase(status) !== 'starting') return null;
  const seconds = startingSeconds(status);
  return {
    seconds,
    text: seconds === null ? 'Aqua is starting…' : `Aqua is starting… ${secondsText(seconds)}`,
    short: seconds === null ? 'starting' : `starting · ${secondsText(seconds)}`,
  };
}

export function audioPipelineReady(status: CoreStatusResponse | null): boolean {
  if (!status) {
    return false;
  }
  const m = status.models;
  return m.deepfilternet_onnx && m.whisper_ggml && m.kokoro_onnx;
}
