const endpoint = process.env.AQUA_CDP_URL ?? 'http://127.0.0.1:9333';
const targets = await fetch(`${endpoint}/json/list`).then((response) => response.json());
const page = targets.find((target) => target.type === 'page' && !target.url.includes('overlay=true')) ?? targets[0];
if (!page?.webSocketDebuggerUrl) throw new Error('The installed Aqua WebView did not expose a page');

const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true });
  socket.addEventListener('error', reject, { once: true });
});

let nextId = 0;
const pending = new Map();
socket.addEventListener('message', ({ data }) => {
  const message = JSON.parse(String(data));
  if (!message.id || !pending.has(message.id)) return;
  const { resolve, reject } = pending.get(message.id);
  pending.delete(message.id);
  if (message.error) reject(new Error(message.error.message));
  else resolve(message.result);
});
const command = (method, params = {}) => new Promise((resolve, reject) => {
  const id = ++nextId;
  pending.set(id, { resolve, reject });
  socket.send(JSON.stringify({ id, method, params }));
});

try {
  const evaluated = await command('Runtime.evaluate', {
    expression: `(async () => {
      const invoke = window.__TAURI_INTERNALS__?.invoke;
      if (!invoke) throw new Error('Tauri IPC bridge is unavailable');
      const gateway = await invoke('gateway_info');
      const health = await fetch(gateway.base_url + '/api/v1/health').then((response) => response.json());
      const falkor = await fetch(gateway.base_url + '/api/v1/falkordb/status', {
        headers: { Authorization: 'Bearer ' + gateway.ui_token },
      }).then((response) => response.json());
      return {
        baseUrl: gateway.base_url,
        tokenPresent: typeof gateway.ui_token === 'string' && gateway.ui_token.length > 20,
        coreStatus: health.status,
        falkorConnected: falkor.connected,
        falkorGraph: falkor.graphName,
        falkorNodes: falkor.nodeCount,
        falkorEdges: falkor.edgeCount,
        title: document.title,
        aclErrorVisible: document.body.innerText.includes('not allowed by ACL'),
      };
    })()`,
    awaitPromise: true,
    returnByValue: true,
  });
  if (evaluated.exceptionDetails) throw new Error(evaluated.exceptionDetails.exception?.description ?? evaluated.exceptionDetails.text);
  const result = evaluated.result.value;
  console.log(JSON.stringify(result, null, 2));
  if (
    result.baseUrl !== 'http://127.0.0.1:8767'
    || !result.tokenPresent
    || result.coreStatus !== 'RUNNING'
    || !result.falkorConnected
    || result.falkorNodes < 1
    || result.aclErrorVisible
  ) {
    process.exitCode = 1;
  }
} finally {
  socket.close();
}
