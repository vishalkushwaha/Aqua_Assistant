// Makes the Browser MCP server listen on the loopback address (127.0.0.1) and on nothing else, and lets only the browser extension (not a web page) open its WebSocket.
//
// Why: Browser MCP 0.1.3 opens its WebSocket with `new WebSocketServer({ port })`: no address, so every network interface, and nobody who connects is checked. With the
// Windows Firewall off, or an allow rule for node.exe, any device on the network could connect, and whoever connects last takes the browser extension's place.
// Aqua refuses to keep such a server running (adapters/mcp_adapters/src/listening.rs). Aqua starts the server with NODE_OPTIONS=--require=<this file>, and this file
// adds the address 127.0.0.1 to the one listen() call that names a port and no address.
//
// Only a listen() for the port in AQUA_LOOPBACK_ONLY_PORT (default 9009) that names no address is changed. A call that names an address, a call for another port, a pipe
// and a file descriptor are passed on untouched. It never stops a server from starting: if anything here fails, the call goes on as it was.
//
// The Origin gate (audit item SE-05). Loopback is not private: any web page open in the person's browser can open `ws://127.0.0.1:9009`, and a browser always says which page
// it comes from in the `Origin` header of the handshake. The extension's own handshake carries `Origin: chrome-extension://<its id>`. On the server that listens on the port
// above, an upgrade request is refused (403, and the socket is closed) before the WebSocket library sees it when it has an Origin that is not an extension:
//   - no Origin header at all is let through: a browser page cannot leave it out, so that is a program on this computer (which can do far worse), and the server is loopback only;
//   - AQUA_BROWSER_MCP_EXTENSION_ID (32 letters a-p, from chrome://extensions) narrows "an extension" to that one extension; Aqua passes it when the person has set it;
//   - AQUA_BROWSER_MCP_ONE_CONNECTION=1 also refuses a second connection while one is open, instead of letting the newest take the first one's place. It is off unless set,
//     because the extension may open a new connection when the person connects another tab and the old one has not closed yet.
// Every refusal is written to stderr (the origin cut to 100 characters, control characters shown as ?). Nothing here reads or changes the traffic of a connection that is let in.
'use strict';

const net = require('net');
const http = require('http');

const PORT = Number(process.env.AQUA_LOOPBACK_ONLY_PORT || 9009);
const LOOPBACK = '127.0.0.1';
const listen = net.Server.prototype.listen;
const emit = http.Server.prototype.emit;
const GATED = Symbol('aqua.originGate');
const EXTENSION_SCHEME = 'chrome-extension://';
const open = new Set();

function isOurPort(value) {
  return (typeof value === 'number' || (typeof value === 'string' && /^\d+$/.test(value))) && Number(value) === PORT;
}

function say(text) {
  try {
    process.stderr.write(`[aqua loopback_only] ${text}\n`);
  } catch (_) {
    // nowhere to say it
  }
}

// What the environment says now (read at each connection, so it is the value the process was started with).
function settings() {
  const raw = String(process.env.AQUA_BROWSER_MCP_EXTENSION_ID || '').trim().toLowerCase();
  const pinned = /^[a-p]{32}$/.test(raw) ? raw : '';
  if (raw && !pinned && !settings.said) {
    settings.said = true;
    say('AQUA_BROWSER_MCP_EXTENSION_ID is not 32 letters a-p: ignored, any extension is let in');
  }
  const one = ['1', 'on', 'true', 'yes'].includes(String(process.env.AQUA_BROWSER_MCP_ONE_CONNECTION || '').trim().toLowerCase());
  return { pinned, one };
}

// Is this Origin one that may open the socket? `undefined` is a request that did not send one.
function originAllowed(origin, pinned) {
  if (origin === undefined) return true;
  const value = String(origin).trim().toLowerCase();
  if (pinned) return value === EXTENSION_SCHEME + pinned;
  return value.startsWith(EXTENSION_SCHEME) && value.length > EXTENSION_SCHEME.length && !/[^a-z0-9._-]/.test(value.slice(EXTENSION_SCHEME.length));
}

function shown(value) {
  return JSON.stringify(String(value).slice(0, 100).replace(/[\u0000-\u001f\u007f]/g, '?'));
}

function refuse(socket, status, why) {
  say(`refused a WebSocket handshake: ${why}`);
  try {
    socket.end(`HTTP/1.1 ${status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
    const t = setTimeout(() => socket.destroy(), 500);
    if (t.unref) t.unref();
  } catch (_) {
    try { socket.destroy(); } catch (__) { /* gone already */ }
  }
}

// Wraps emit() of HTTP servers, but only acts on 'upgrade' for the server that listens on our port.
http.Server.prototype.emit = function (event, ...args) {
  if (event === 'upgrade' && this[GATED]) {
    try {
      const [req, socket] = args;
      const { pinned, one } = settings();
      const origin = req && req.headers ? req.headers.origin : undefined;
      if (!originAllowed(origin, pinned)) {
        refuse(socket, '403 Forbidden', `Origin ${shown(origin)} is not ${pinned ? `the extension ${pinned}` : 'a browser extension'}`);
        return true;
      }
      if (one && open.size > 0) {
        refuse(socket, '409 Conflict', 'another connection is open and AQUA_BROWSER_MCP_ONE_CONNECTION is on');
        return true;
      }
      open.add(socket);
      socket.once('close', () => open.delete(socket));
    } catch (_) {
      // a failure of the gate must not stop the server: the upgrade goes on as it was
    }
  }
  return emit.call(this, event, ...args);
};

net.Server.prototype.listen = function (...args) {
  try {
    const first = args[0];
    if (isOurPort(first)) {
      this[GATED] = true;
      // listen(port[, host][, backlog][, callback])
      if (args[1] === undefined || args[1] === null) {
        args[1] = LOOPBACK;
      } else if (typeof args[1] === 'function' || typeof args[1] === 'number') {
        args.splice(1, 0, LOOPBACK);
      }
    } else if (first && typeof first === 'object' && !first.path && first.fd === undefined && isOurPort(first.port)) {
      this[GATED] = true;
      if (!first.host) {
        // listen({ port, ... }[, callback])
        args[0] = Object.assign({}, first, { host: LOOPBACK });
      }
    }
  } catch (_) {
    // leave the call as it was
  }
  return listen.apply(this, args);
};
