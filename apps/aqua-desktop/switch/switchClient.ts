import { randomUUID } from 'node:crypto';
import WebSocket, { type RawData } from 'ws';

export type SwitchConnectionScope = 'single' | 'all';
export type SwitchConnectionFilter = 'all' | 'addressed';
export type SwitchAddressing = 'owner_only' | 'owner_and_owner_agents' | 'anyone';

export interface SwitchRegistrationRequest {
  name: string;
  displayName?: string;
  description: string;
  connectorType: string;
  overwrite?: boolean;
}

export interface SwitchRegistration {
  id: string;
  apiKey: string;
}

export interface SwitchMessagePayload {
  addressed: boolean;
  sender: string;
  sender_name: string;
  message_id: string;
  body: string;
  timestamp: number;
  thread_id: string | null;
  attachments: Array<Record<string, unknown>>;
}

export interface SwitchDomainEvent {
  event: 'message' | 'command' | 'room_join';
  id: number;
  data: {
    type: 'message' | 'command' | 'room_join';
    room_id: string;
    bridge_id: string | null;
    channel_type: string | null;
    payload: SwitchMessagePayload | Record<string, unknown>;
    sequence: number;
  };
}

export interface SwitchClientOptions {
  bridgeUrl: string;
  agentId: string;
  apiKey: string;
  roomIds?: string[];
  connectionId?: string;
  initialCursor?: number | null;
  initialGeneration?: number | null;
  scope?: SwitchConnectionScope;
  filter?: SwitchConnectionFilter;
  clientName?: string;
  clientVersion?: string;
  reconnectMinimumMs?: number;
  reconnectMaximumMs?: number;
  onCheckpoint?: (checkpoint: SwitchConnectionCheckpoint) => void;
  onControlFrame?: (frame: Readonly<SwitchControlFrame>) => void;
}

export interface SwitchConnectionCheckpoint {
  connectionId: string;
  cursor: number | null;
  generation: number | null;
}

interface NormalizedSwitchClientOptions {
  bridgeUrl: string;
  agentId: string;
  apiKey: string;
  roomIds: string[];
  scope: SwitchConnectionScope;
  filter: SwitchConnectionFilter;
  clientName: string;
  clientVersion: string;
  reconnectMinimumMs: number;
  reconnectMaximumMs: number;
}

export interface SwitchControlFrame {
  type: string;
  [key: string]: unknown;
}

export class SwitchProtocolError extends Error {
  readonly status: number;
  readonly code: string | null;

  constructor(message: string, status: number, code: string | null = null) {
    super(message);
    this.name = 'SwitchProtocolError';
    this.status = status;
    this.code = code;
  }
}

function baseUrl(value: string): string {
  const url = new URL(value);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') throw new Error('Switch bridge URL must use http or https.');
  return url.toString().replace(/\/$/, '');
}

async function responseJson(response: Response): Promise<Record<string, unknown>> {
  const text = await response.text();
  if (!text) return {};
  try {
    const value = JSON.parse(text) as unknown;
    return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {};
  } catch {
    throw new SwitchProtocolError(`Switch returned a non-JSON response (${response.status}).`, response.status);
  }
}

async function requireSuccess(response: Response): Promise<Record<string, unknown>> {
  const body = await responseJson(response);
  if (response.ok) return body;
  const detail = typeof body.detail === 'string' ? body.detail : typeof body.error === 'string' ? body.error : `Switch request failed (${response.status}).`;
  throw new SwitchProtocolError(detail, response.status, typeof body.code === 'string' ? body.code : null);
}

export async function registerSwitchAgent(
  bridgeUrl: string,
  registrationToken: string,
  request: SwitchRegistrationRequest,
): Promise<SwitchRegistration> {
  const response = await fetch(`${baseUrl(bridgeUrl)}/agents`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${registrationToken}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      name: request.name,
      display_name: request.displayName,
      description: request.description,
      connector_type: request.connectorType,
      overwrite: request.overwrite || false,
      integration_profile: {
        connection_model: 'always_on',
        message_exchange: true,
        pre_invocation_mediation: [],
        post_invocation_mediation: [],
        event_reporting: [],
      },
      tools: ['FalkorDB Company Brain'],
      metadata: { project: 'Aqua Company Brain', transport: 'agent-protocol-websocket' },
    }),
  });
  const body = await requireSuccess(response);
  if (typeof body.id !== 'string' || typeof body.api_key !== 'string') {
    throw new SwitchProtocolError('Switch registration did not return an agent id and API key.', response.status);
  }
  return { id: body.id, apiKey: body.api_key };
}

export class SwitchClient {
  readonly connectionId: string;
  private readonly options: NormalizedSwitchClientOptions;
  private readonly onCheckpoint?: SwitchClientOptions['onCheckpoint'];
  private readonly onControlFrame?: SwitchClientOptions['onControlFrame'];
  private cursor: number | null;
  private generation: number | null;
  private socket: WebSocket | null = null;
  private stopped = false;

  constructor(options: SwitchClientOptions) {
    this.connectionId = options.connectionId || randomUUID();
    this.cursor = options.initialCursor ?? null;
    this.generation = options.initialGeneration ?? null;
    this.onCheckpoint = options.onCheckpoint;
    this.onControlFrame = options.onControlFrame;
    this.options = {
      bridgeUrl: baseUrl(options.bridgeUrl),
      agentId: options.agentId,
      apiKey: options.apiKey,
      roomIds: options.roomIds || [],
      scope: options.scope || 'single',
      filter: options.filter || 'addressed',
      clientName: options.clientName || 'aqua-company-brain',
      clientVersion: options.clientVersion || '0.1.0',
      reconnectMinimumMs: options.reconnectMinimumMs || 500,
      reconnectMaximumMs: options.reconnectMaximumMs || 15_000,
    };
  }

  async discoverOperations(): Promise<Record<string, unknown>> {
    const body = await this.request(`/agents/${encodeURIComponent(this.options.agentId)}/ops`);
    return body.operations && typeof body.operations === 'object' ? body.operations as Record<string, unknown> : {};
  }

  async invokeOperation<T = unknown>(operation: string, argumentsObject: Record<string, unknown>): Promise<T> {
    const body = await this.request(`/agents/${encodeURIComponent(this.options.agentId)}/ops/${encodeURIComponent(operation)}`, {
      method: 'POST',
      headers: { 'X-Switch-Connection-Id': this.connectionId },
      body: JSON.stringify(argumentsObject),
    });
    return body.result as T;
  }

  async postMessage(roomId: string, content: string): Promise<Record<string, unknown>> {
    return this.request(`/agents/${encodeURIComponent(this.options.agentId)}/message`, {
      method: 'POST',
      body: JSON.stringify({ room_id: roomId, content }),
    });
  }

  async updateAddressing(addressing: SwitchAddressing): Promise<unknown> {
    return this.invokeOperation('update_agent_detail', { agent_id: this.options.agentId, addressing });
  }

  async run(handler: (event: SwitchDomainEvent, client: SwitchClient) => Promise<void> | void, signal?: AbortSignal): Promise<void> {
    this.stopped = false;
    let retryMs = this.options.reconnectMinimumMs;
    while (!this.stopped && !signal?.aborted) {
      const outcome = await this.connectOnce(handler, signal);
      if (!outcome.reconnect || this.stopped || signal?.aborted) return;
      await this.delay(retryMs, signal);
      retryMs = outcome.connected ? this.options.reconnectMinimumMs : Math.min(retryMs * 2, this.options.reconnectMaximumMs);
    }
  }

  close(): void {
    this.stopped = true;
    this.socket?.close(1000, 'Aqua Switch client stopped');
    this.socket = null;
  }

  private async request(path: string, init: RequestInit = {}): Promise<Record<string, unknown>> {
    const headers = new Headers(init.headers);
    headers.set('Authorization', `Bearer ${this.options.apiKey}`);
    if (init.body !== undefined) headers.set('Content-Type', 'application/json');
    const response = await fetch(`${this.options.bridgeUrl}${path}`, { ...init, headers });
    return requireSuccess(response);
  }

  private socketUrl(): string {
    const url = new URL(this.options.bridgeUrl);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    url.pathname = `${url.pathname.replace(/\/$/, '')}/agents/${encodeURIComponent(this.options.agentId)}/connection/ws`;
    url.search = '';
    url.searchParams.set('connection_id', this.connectionId);
    url.searchParams.set('scope', this.options.scope);
    url.searchParams.set('filter', this.options.filter);
    url.searchParams.set('start_from', this.cursor === null ? 'head' : String(this.cursor));
    url.searchParams.set('client', this.options.clientName);
    url.searchParams.set('client_version', this.options.clientVersion);
    if (this.options.roomIds.length > 0) url.searchParams.set('rooms', this.options.roomIds.join(','));
    if (this.generation !== null) url.searchParams.set('expected_generation', String(this.generation));
    return url.toString();
  }

  private connectOnce(
    handler: (event: SwitchDomainEvent, client: SwitchClient) => Promise<void> | void,
    signal?: AbortSignal,
  ): Promise<{ reconnect: boolean; connected: boolean }> {
    return new Promise((resolve, reject) => {
      let settled = false;
      let connected = false;
      let reconnect = true;
      let processing = Promise.resolve();
      const socket = new WebSocket(this.socketUrl(), { headers: { Authorization: `Bearer ${this.options.apiKey}` } });
      this.socket = socket;

      const abort = () => {
        reconnect = false;
        socket.close(1000, 'Aqua Switch client stopped');
        finish();
      };
      const finish = () => {
        if (settled) return;
        settled = true;
        signal?.removeEventListener('abort', abort);
        processing.then(() => resolve({ reconnect, connected }), reject);
      };
      signal?.addEventListener('abort', abort, { once: true });

      socket.on('message', (raw: RawData) => {
        let frame: SwitchControlFrame | SwitchDomainEvent;
        try {
          frame = JSON.parse(raw.toString()) as SwitchControlFrame | SwitchDomainEvent;
        } catch {
          socket.close(1002, 'Invalid JSON frame');
          return;
        }

        if ('event' in frame && typeof frame.event === 'string' && 'data' in frame) {
          const event = frame as SwitchDomainEvent;
          processing = processing.then(async () => {
            await handler(event, this);
            this.cursor = Math.max(this.cursor || 0, event.id);
            this.checkpoint();
          }).catch((error: unknown) => {
            socket.close(1011, 'Aqua event handler failed');
            throw error;
          });
          return;
        }

        const control = frame as SwitchControlFrame;
        try {
          this.onControlFrame?.(control);
        } catch (error) {
          console.warn('[switch] Control-frame observer failed:', error);
        }
        if (control.type === 'ping') {
          socket.send(JSON.stringify({ type: 'pong', cursor: this.cursor || 0 }));
        } else if (control.type === 'connection_state') {
          connected = true;
          if (typeof control.generation === 'number') this.generation = control.generation;
          if (typeof control.cursor === 'number') this.cursor = control.cursor;
          this.checkpoint();
        } else if (control.type === 'gap' && typeof control.resumed_at === 'number') {
          this.cursor = control.resumed_at;
          this.checkpoint();
        } else if (control.type === 'evicted' && control.code === 'taken_over') {
          reconnect = false;
        } else if (control.type === 'refused') {
          reconnect = false;
        }
      });
      socket.once('close', (code) => {
        if (code === 4401 || code === 4403 || code === 4409) reconnect = false;
        if (this.socket === socket) this.socket = null;
        finish();
      });
      socket.once('error', (error) => {
        if (this.socket === socket) this.socket = null;
        if (!connected) console.warn(`[switch] Connection failed: ${error.message}`);
      });
    });
  }

  private delay(milliseconds: number, signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.resolve();
    return new Promise((resolve) => {
      const done = () => {
        signal?.removeEventListener('abort', done);
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(done, milliseconds);
      signal?.addEventListener('abort', done, { once: true });
    });
  }

  private checkpoint(): void {
    try {
      this.onCheckpoint?.({ connectionId: this.connectionId, cursor: this.cursor, generation: this.generation });
    } catch (error) {
      console.warn('[switch] Could not persist the connection checkpoint:', error);
    }
  }
}
