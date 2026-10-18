const endpoint = process.env.AQUA_CDP_URL ?? 'http://127.0.0.1:9335';
const targets = await fetch(`${endpoint}/json/list`).then((response) => response.json());
const page = targets.find((target) => target.type === 'page' && !target.url.includes('overlay=true')) ?? targets[0];
if (!page?.webSocketDebuggerUrl) throw new Error('The Aqua WebView did not expose the admin page');

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
  const request = pending.get(message.id);
  pending.delete(message.id);
  if (message.error) request.reject(new Error(message.error.message));
  else request.resolve(message.result);
});
const command = (method, params = {}) => new Promise((resolve, reject) => {
  const id = ++nextId;
  pending.set(id, { resolve, reject });
  socket.send(JSON.stringify({ id, method, params }));
});

try {
  const evaluated = await command('Runtime.evaluate', {
    expression: `(async () => {
      const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
      const runtimeErrors = [];
      window.addEventListener('error', (event) => runtimeErrors.push(String(event.error?.stack || event.message)));
      window.addEventListener('unhandledrejection', (event) => runtimeErrors.push(String(event.reason?.stack || event.reason)));
      const results = [];
      for (const button of document.querySelectorAll('button.nav-item')) {
        button.click();
        await delay(1300);
        const text = document.querySelector('main')?.innerText || '';
        const suspicious = text.split('\\n').filter((line) =>
          /unexpected token|json|syntaxerror|doctype|cannot read propert|is not valid/i.test(line)
          || line.includes('[object Object]')
        );
        results.push({ page: button.innerText.trim(), suspicious });
      }
      return { results, runtimeErrors };
    })()`,
    awaitPromise: true,
    returnByValue: true,
  });
  if (evaluated.exceptionDetails) throw new Error(evaluated.exceptionDetails.exception?.description ?? evaluated.exceptionDetails.text);
  console.log(JSON.stringify(evaluated.result.value, null, 2));
} finally {
  socket.close();
}
