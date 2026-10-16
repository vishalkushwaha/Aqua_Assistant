import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { falkorPlugin } from './falkorPlugin';

// aqua-core writes a per-install API token here on first start (see core/src/api_gateway/auth.rs).
const TOKEN_FILE = fileURLToPath(new URL('../../data/.aqua_api_token', import.meta.url));
// A second token that only this proxy holds. It alone can answer approval prompts and change policy.
const UI_TOKEN_FILE = fileURLToPath(new URL('../../data/.aqua_ui_token', import.meta.url));
const DOTENV_FILE = fileURLToPath(new URL('./.env', import.meta.url));

// Vite follows dotenv comment rules, so an unquoted `#` inside a password is
// silently treated as the start of a comment. Read this server-only value
// verbatim; it is passed only to the development bridge and never bundled.
function rawDotenvValue(name: string): string {
  try {
    const prefix = `${name}=`;
    const line = readFileSync(DOTENV_FILE, 'utf8')
      .split(/\r?\n/)
      .find((candidate) => candidate.startsWith(prefix));
    if (!line) return '';
    const raw = line.slice(prefix.length).trim();
    if (raw.length >= 2 && ((raw.startsWith('"') && raw.endsWith('"')) || (raw.startsWith("'") && raw.endsWith("'")))) {
      return raw.slice(1, -1);
    }
    return raw;
  } catch {
    return '';
  }
}

// Read per request so starting Vite before aqua-core still works once the token exists.
function readToken(envName: string, file: string): string {
  const fromEnv = process.env[envName]?.trim();
  if (fromEnv) return fromEnv;
  try {
    return readFileSync(file, 'utf8').trim();
  } catch {
    return '';
  }
}

// The browser only ever talks to this proxy, which sends the UI token. If aqua-core has not
// written it yet, fall back to the API token: everything works except approving.
function apiToken(): string {
  return readToken('AQUA_UI_TOKEN', UI_TOKEN_FILE) || readToken('AQUA_API_TOKEN', TOKEN_FILE);
}

// Only this app's own page may borrow the token. A browser tells us where a request comes from (`Origin`, or
// `Sec-Fetch-Site` for same-origin GETs); another program on this computer, or a page on another site, does not look
// like that, and without this check anything that could reach port 1420 would act with the UI token (approving its own
// actions). Development only: the packaged app hands the token to its own page directly.
const DEV_ORIGINS = new Set(['http://127.0.0.1:1420', 'http://localhost:1420']);
function fromThisPage(headers: Record<string, string | string[] | undefined>): boolean {
  const origin = headers['origin'];
  if (typeof origin === 'string' && origin) return DEV_ORIGINS.has(origin);
  const site = headers['sec-fetch-site'];
  return site === 'same-origin' || site === 'none';
}

export default defineConfig(({ mode }) => {
  // Vite loads .env after evaluating the config, so explicitly pass server-only FalkorDB
  // values to the development bridge. These values are never exposed through import.meta.env.
  const env = loadEnv(mode, process.cwd(), 'FALKORDB_');
  const falkorPort = Number(env.FALKORDB_PORT);

  return {
  plugins: [react(), falkorPlugin({
    host: env.FALKORDB_HOST || undefined,
    port: Number.isFinite(falkorPort) && falkorPort > 0 ? falkorPort : undefined,
    username: env.FALKORDB_USER || undefined,
    password: process.env.FALKORDB_PASSWORD || rawDotenvValue('FALKORDB_PASSWORD') || env.FALKORDB_PASSWORD || undefined,
    graphName: env.FALKORDB_GRAPH || undefined,
    tls: env.FALKORDB_TLS ? env.FALKORDB_TLS.toLowerCase() === 'true' : undefined,
    allowInsecureRemote: env.FALKORDB_ALLOW_INSECURE_REMOTE
      ? env.FALKORDB_ALLOW_INSECURE_REMOTE.toLowerCase() === 'true'
      : undefined,
  })],
  clearScreen: false,
  build: {
    rollupOptions: {
      output: {
        manualChunks: {
          'react-vendor': ['react', 'react-dom'],
          'icon-vendor': ['lucide-react'],
        },
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    // Loopback only: the proxy below carries the API token, so it must not be reachable from the LAN.
    host: '127.0.0.1',
    watch: {
      ignored: ['**/src-tauri/**']
    },
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8767',
        changeOrigin: true,
        bypass: (req) => {
          if (req.url && req.url.startsWith('/api/falkordb')) {
            return req.url;
          }
        },
        // Agent SSE must not be buffered or the UI freezes on the first "Plan…" line.
        timeout: 0,
        proxyTimeout: 0,
        configure: (proxy) => {
          proxy.on('proxyReq', (proxyReq, req) => {
            const token = apiToken();
            if (token && fromThisPage(req.headers)) proxyReq.setHeader('Authorization', `Bearer ${token}`);
          });
          proxy.on('proxyRes', (proxyRes, req) => {
            const url = req.url || '';
            const ct = String(proxyRes.headers['content-type'] || '');
            if (url.includes('/stream') || ct.includes('text/event-stream')) {
              proxyRes.headers['cache-control'] = 'no-cache, no-transform';
              proxyRes.headers['x-accel-buffering'] = 'no';
              proxyRes.headers['connection'] = 'keep-alive';
            }
          });
        },
      },
      '/ws': {
        target: 'ws://127.0.0.1:8767',
        ws: true,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyReqWs', (proxyReq, req) => {
            const token = apiToken();
            if (token && fromThisPage(req.headers)) proxyReq.setHeader('Authorization', `Bearer ${token}`);
          });
        },
      },
    },
  },
  };
});
