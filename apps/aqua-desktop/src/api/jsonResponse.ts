/** A response problem already worded for display in the desktop UI. */
export class JsonResponseError extends Error {
  readonly status: number | null;

  constructor(message: string, status: number | null) {
    super(message);
    this.name = 'JsonResponseError';
    this.status = status;
  }
}

const cleanMessage = (value: unknown): string => {
  if (typeof value !== 'string') return '';
  const message = value.replace(/\s+/g, ' ').trim();
  return message.length > 0 && message.length <= 240 ? message : '';
};

/** A short server explanation, without ever exposing an HTML/error document as page text. */
export function responseErrorMessage(body: unknown): string {
  if (!body || typeof body !== 'object' || Array.isArray(body)) return '';
  const record = body as Record<string, unknown>;
  return cleanMessage(record.error) || cleanMessage(record.message) || cleanMessage(record.detail);
}

/** Empty bodies are null; malformed successful replies become a stable UI error. */
export async function readResponseJson(response: Response): Promise<unknown | null> {
  const text = await response.text();
  if (text.trim() === '') return null;
  try {
    return JSON.parse(text.replace(/^\uFEFF/, '')) as unknown;
  } catch {
    if (!response.ok) return null;
    throw new JsonResponseError('Aqua sent a response the app could not read. Please try again.', response.status);
  }
}

/** Read a required JSON response and turn every failure into a short, safe sentence. */
export async function requireJson<T>(response: Response, fallback?: string): Promise<T> {
  const body = await readResponseJson(response);
  if (!response.ok) {
    const detail = responseErrorMessage(body);
    const unavailable = response.status >= 500
      ? 'Aqua Core is still starting. This panel will reconnect automatically.'
      : '';
    throw new JsonResponseError(detail || unavailable || fallback || 'Aqua could not complete this request.', response.status);
  }
  if (body === null) throw new JsonResponseError('Aqua sent an empty response. Please try again.', response.status);
  return body as T;
}
