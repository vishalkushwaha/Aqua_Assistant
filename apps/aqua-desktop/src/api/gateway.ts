// In development the Vite proxy forwards /api to aqua-core and adds the tokens. A packaged app has no proxy, so
// this does the same job inside the page: it asks the desktop shell for the gateway address and the UI token
// (read from Aqua's own data folder; the page never stores it) and applies them to every /api request.

interface GatewayInfo {
  base_url: string;
  ui_token: string;
}

type TauriWindow = Window & { __TAURI_INTERNALS__?: { invoke: (cmd: string, args?: unknown) => Promise<unknown> } };

/** True only inside the packaged desktop app, never in the browser or the dev server. */
export function isPackagedApp(): boolean {
  return import.meta.env.PROD && Boolean((window as TauriWindow).__TAURI_INTERNALS__);
}

/** Pure: where a request should go, and whether it should carry the token. Only our own /api paths do. */
export function rewrite(url: string, info: GatewayInfo): { url: string; token: string | null } {
  // FalkorDB Cloud Graph bridge is hosted on port 1420
  if (url.startsWith('/api/falkordb')) return { url: 'http://127.0.0.1:1420' + url, token: null };
  if (url.startsWith('http://127.0.0.1:1420/api/falkordb')) return { url, token: null };

  if (url.startsWith('/api/')) return { url: info.base_url + url, token: info.ui_token };
  if (url.startsWith(info.base_url + '/api/')) return { url, token: info.ui_token };
  return { url, token: null };
}

async function readGatewayInfo(): Promise<GatewayInfo> {
  const tauri = (window as TauriWindow).__TAURI_INTERNALS__;
  if (!tauri) throw new Error('not running inside the desktop app');
  // The core writes its tokens when it first starts, which can be a few seconds after the window opens.
  let lastError: unknown = null;
  for (let attempt = 0; attempt < 60; attempt++) {
    try {
      return (await tauri.invoke('gateway_info')) as GatewayInfo;
    } catch (e) {
      lastError = e;
      await new Promise((r) => setTimeout(r, 1000));
    }
  }
  throw new Error(`Aqua's core did not start: ${String(lastError)}`);
}

/** Call once before the app renders. Does nothing outside the packaged app. */
export async function installGatewayBridge(): Promise<void> {
  if (!isPackagedApp()) return;
  const info = await readGatewayInfo();
  const original = window.fetch.bind(window);
  window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    const asText = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
    const target = rewrite(asText, info);
    if (target.token === null) return original(input, init);
    const headers = new Headers(init?.headers ?? (typeof input === 'object' && 'headers' in input ? input.headers : undefined));
    if (!headers.has('Authorization')) headers.set('Authorization', `Bearer ${target.token}`);
    return original(target.url, { ...init, headers });
  };
}

/** Open a web address in the person's own browser. A webview does nothing with `target="_blank"`, so the desktop shell
 *  does it (and only for http and https); a normal browser just opens a tab. */
export function openExternal(url: string): void {
  if (!/^https?:\/\//i.test(url)) return;
  const tauri = (window as TauriWindow).__TAURI_INTERNALS__;
  if (tauri) {
    void tauri.invoke('open_external', { url }).catch(() => undefined);
  } else {
    window.open(url, '_blank', 'noopener,noreferrer');
  }
}
