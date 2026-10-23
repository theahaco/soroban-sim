// soroban-sim virtual RPC.
//
// A page's stellar-sdk (or any fetch) can point at
//   <scope>__soroban-sim/rpc/<session-id>
// and get stellar-rpc's JSON-RPC answers from a network session running in
// this browser. The service worker holds no session: it relays each request
// body to the tab that does (which runs the component in a Web Worker) and
// returns that tab's answer. Nothing here talks to a real network.
//
// Plain JavaScript so the same file serves from Vite's dev server and from the
// static build; the message shapes are in src/session-protocol.ts.

const PREFIX = "__soroban-sim/rpc/";
const TIMEOUT_MS = 120_000;
const JSON_HEADERS = { "content-type": "application/json", "cache-control": "no-store" };

self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  const base = new URL(self.registration.scope).pathname;
  if (!url.pathname.startsWith(base + PREFIX)) return;
  const sessionId = decodeURIComponent(url.pathname.slice((base + PREFIX).length));
  event.respondWith(relay(event, sessionId));
});

/** One JSON-RPC error response, for requests no tab can answer. */
function rpcError(message, code = -32603) {
  return new Response(JSON.stringify({ jsonrpc: "2.0", id: null, error: { code, message } }), {
    status: 200,
    headers: JSON_HEADERS,
  });
}

async function relay(event, sessionId) {
  if (event.request.method !== "POST") {
    return new Response("soroban-sim virtual RPC: POST a JSON-RPC body", { status: 405 });
  }
  const body = await event.request.text();
  for (const client of await candidates(event.clientId)) {
    const answer = await ask(client, sessionId, body);
    if (answer && answer.held) {
      return new Response(answer.body ?? "", { status: answer.body ? 200 : 204, headers: JSON_HEADERS });
    }
  }
  return rpcError(`no open soroban-sim tab holds session ${sessionId}; open it in the session panel first`);
}

/** The requesting tab first, then every other tab of this origin. */
async function candidates(clientId) {
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  const first = clientId ? windows.find((w) => w.id === clientId) : undefined;
  return first ? [first, ...windows.filter((w) => w !== first)] : windows;
}

/** Ask one tab; resolves `{ held, body }`, or undefined if it does not answer. */
function ask(client, sessionId, body) {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timer = setTimeout(() => resolve(undefined), TIMEOUT_MS);
    channel.port1.onmessage = (message) => {
      clearTimeout(timer);
      resolve(message.data);
    };
    client.postMessage({ type: "soroban-sim/rpc", sessionId, body }, [channel.port2]);
  });
}
