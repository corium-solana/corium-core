/**
 * Browser-facing Solana RPC proxy.
 *
 * The live client used to talk to Helius directly. That forced a choice
 * between shipping the API key in the bundle and using a masked "secure" URL,
 * which is IP rate-limited to 5 req/s and serves no websockets - so the boot
 * burst tripped 429s and `onLogs` never connected. Worse, Helius omits CORS
 * headers on 429, so the browser reported every throttle as a CORS failure and
 * hid the real status.
 *
 * This proxy holds the key server-side and fixes all three: it speaks CORS on
 * every response including errors, absorbs upstream throttles with a retry,
 * and bridges websockets to the keyed endpoint that actually serves them.
 *
 * It is deliberately not an open relay: only the JSON-RPC methods the client
 * needs are forwarded, and each IP gets its own token bucket.
 */

import * as http from 'http';
import * as path from 'path';

import * as dotenv from 'dotenv';
import { WebSocket, WebSocketServer } from 'ws';

import {
  CHAT_MAX_BODY,
  canonicalTs,
  chatRowFresh,
  normalizeChatBody,
  verifyChatRow,
} from '../../shared/chain/chat.js';
import { historyCluster } from '../../shared/chain/config.js';

dotenv.config({ path: path.resolve(__dirname, '..', '.env') });
dotenv.config({ path: path.resolve(__dirname, '..', '..', '.env') });

/** Helius and friends keep the API key on the query string; keep it on wss too. */
function rpcToWs(url: string): string {
  if (url.startsWith('https://')) return `wss://${url.slice('https://'.length)}`;
  if (url.startsWith('http://')) return `ws://${url.slice('http://'.length)}`;
  return url;
}

const UPSTREAM_HTTP = process.env.SOLANA_RPC_URL || 'https://api.devnet.solana.com';
const UPSTREAM_WS = process.env.SOLANA_WS_URL || rpcToWs(UPSTREAM_HTTP);
const PORT = Number(process.env.PORT || 8080);

/** Empty means allow any origin - fine for a read-mostly devnet proxy. */
const ALLOWED_ORIGINS = (process.env.PROXY_ALLOWED_ORIGINS || '')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);

/** Per-IP token bucket. Sized to let a page boot without a stall. */
const RATE_BURST = Number(process.env.PROXY_RATE_BURST || 60);
const RATE_REFILL_PER_SEC = Number(process.env.PROXY_RATE_REFILL || 15);
const MAX_WS_PER_IP = Number(process.env.PROXY_MAX_WS_PER_IP || 4);

const MAX_BODY_BYTES = 512 * 1024;
const MAX_BATCH = 100;
const UPSTREAM_TIMEOUT_MS = 20_000;
const UPSTREAM_ATTEMPTS = 3;

// ----------------------------------------------------------------------- chat
//
// The one writer for `corium_chat`. The table used to take anonymous inserts
// with the signature checked only in the browser, which meant the signature
// gated nothing: the anon key ships in the bundle, so anyone could post rows,
// and ~50 junk rows for a star pushed every real message out of the read window.
// Verifying here and holding the service role means an unsigned row cannot be
// written at all. Reads stay on Supabase directly - they are harmless and this
// process should not carry the polling traffic.

const SUPABASE_URL = (process.env.SUPABASE_URL || process.env.VITE_SUPABASE_URL || '').replace(
  /\/$/,
  ''
);
const SUPABASE_SERVICE_KEY = process.env.SUPABASE_SERVICE_ROLE_KEY || '';
const CHAT_ENABLED = !!(SUPABASE_URL && SUPABASE_SERVICE_KEY);
/** Toy vs prod share one project; the proxy stamps this so a client cannot pick. */
const CHAT_CLUSTER = historyCluster();

/** A chat message is a few hundred bytes; the JSON-RPC cap is far too generous. */
const CHAT_MAX_BODY_BYTES = 4 * 1024;

/**
 * Everything the live client and its Anchor/web3.js internals reach for.
 * `getTransactions` (plural) arrives as a JSON-RPC batch of `getTransaction`.
 */
const HTTP_METHODS = new Set([
  'getAccountInfo',
  'getMultipleAccounts',
  'getProgramAccounts',
  'getBalance',
  'getMinimumBalanceForRentExemption',
  'getLatestBlockhash',
  'getRecentBlockhash',
  'getFeeForMessage',
  'getSignatureStatuses',
  'getSignaturesForAddress',
  'getTransaction',
  'getBlockTime',
  'getSlot',
  'getBlockHeight',
  'getEpochInfo',
  'getGenesisHash',
  'getVersion',
  'simulateTransaction',
  'sendTransaction',
]);

const WS_METHODS = new Set([
  'logsSubscribe',
  'logsUnsubscribe',
  'accountSubscribe',
  'accountUnsubscribe',
  'programSubscribe',
  'programUnsubscribe',
  'signatureSubscribe',
  'signatureUnsubscribe',
  'slotSubscribe',
  'slotUnsubscribe',
  'rootSubscribe',
  'rootUnsubscribe',
]);

/** Never let the upstream URL (and its key) reach a client or a log line. */
function redact(err: unknown): string {
  const msg = err instanceof Error ? err.message : String(err);
  return msg.split(UPSTREAM_HTTP).join('<upstream>').replace(/api[-_]?key=[^&\s]+/gi, 'api-key=***');
}

function upstreamHost(): string {
  try {
    return new URL(UPSTREAM_HTTP).host;
  } catch {
    return 'invalid';
  }
}

function clientIp(req: http.IncomingMessage): string {
  const cf = req.headers['cf-connecting-ip'];
  if (typeof cf === 'string' && cf) return cf;
  const fly = req.headers['fly-client-ip'];
  if (typeof fly === 'string' && fly) return fly;
  const fwd = req.headers['x-forwarded-for'];
  if (typeof fwd === 'string' && fwd) return fwd.split(',')[0].trim();
  return req.socket.remoteAddress || 'unknown';
}

function originAllowed(origin: string | undefined): boolean {
  if (!ALLOWED_ORIGINS.length) return true;
  if (!origin) return true; // curl, server-side callers: no CORS to satisfy
  return ALLOWED_ORIGINS.includes(origin);
}

// ------------------------------------------------------------------ throttle

type Bucket = { tokens: number; last: number };
const buckets = new Map<string, Bucket>();

function takeToken(ip: string): boolean {
  const now = Date.now();
  const bucket = buckets.get(ip) ?? { tokens: RATE_BURST, last: now };
  const refill = ((now - bucket.last) / 1000) * RATE_REFILL_PER_SEC;
  bucket.tokens = Math.min(RATE_BURST, bucket.tokens + refill);
  bucket.last = now;
  if (bucket.tokens < 1) {
    buckets.set(ip, bucket);
    return false;
  }
  bucket.tokens -= 1;
  buckets.set(ip, bucket);
  return true;
}

/** Buckets at full charge carry no state worth keeping. */
setInterval(() => {
  const now = Date.now();
  for (const [ip, bucket] of buckets) {
    if (now - bucket.last > 60_000) buckets.delete(ip);
  }
}, 60_000).unref();

// ----------------------------------------------------------------- responses

function corsHeaders(origin: string | undefined): Record<string, string> {
  const headers: Record<string, string> = {
    'Access-Control-Allow-Methods': 'POST, GET, OPTIONS',
    'Access-Control-Allow-Headers': 'content-type, solana-client, x-requested-with',
    'Access-Control-Max-Age': '86400',
    Vary: 'Origin',
  };
  if (!ALLOWED_ORIGINS.length) headers['Access-Control-Allow-Origin'] = '*';
  else if (origin && ALLOWED_ORIGINS.includes(origin)) {
    headers['Access-Control-Allow-Origin'] = origin;
  }
  return headers;
}

/**
 * Always CORS-tagged, including on 429 and 5xx. This is the whole point: a
 * response the browser cannot read shows up as a CORS error and buries the
 * actual failure.
 */
function send(
  res: http.ServerResponse,
  origin: string | undefined,
  status: number,
  body: string,
  extra: Record<string, string> = {}
) {
  res.writeHead(status, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
    ...corsHeaders(origin),
    ...extra,
  });
  res.end(body);
}

function rpcError(id: unknown, code: number, message: string): string {
  return JSON.stringify({ jsonrpc: '2.0', id: id ?? null, error: { code, message } });
}

// ---------------------------------------------------------------- validation

type RpcCall = { id?: unknown; method?: unknown };

/** Returns null when the payload is acceptable, else a client-safe reason. */
function rejectPayload(payload: unknown): string | null {
  const calls: RpcCall[] = Array.isArray(payload) ? payload : [payload as RpcCall];
  if (!calls.length) return 'empty batch';
  if (calls.length > MAX_BATCH) return `batch too large (max ${MAX_BATCH})`;
  for (const call of calls) {
    if (!call || typeof call !== 'object') return 'malformed request';
    if (typeof call.method !== 'string') return 'missing method';
    if (!HTTP_METHODS.has(call.method)) return `method not allowed: ${call.method}`;
  }
  return null;
}

// ------------------------------------------------------------------ upstream

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

type UpstreamResult = { status: number; body: string; retryAfter?: string };

/**
 * Upstream throttles are transient and the client cannot retry a 429 it is not
 * allowed to read, so absorb a couple of them here before giving up.
 */
async function forward(body: string): Promise<UpstreamResult> {
  let last: UpstreamResult = { status: 502, body: rpcError(null, -32603, 'upstream unreachable') };

  for (let attempt = 0; attempt < UPSTREAM_ATTEMPTS; attempt += 1) {
    if (attempt) await sleep(120 * 3 ** (attempt - 1));

    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), UPSTREAM_TIMEOUT_MS);
    try {
      const upstream = await fetch(UPSTREAM_HTTP, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
        signal: abort.signal,
      });
      const text = await upstream.text();
      if (upstream.status !== 429 && upstream.status < 500) {
        return { status: upstream.status, body: text };
      }
      last = {
        status: upstream.status,
        body: text || rpcError(null, -32603, `upstream ${upstream.status}`),
        retryAfter: upstream.headers.get('retry-after') ?? undefined,
      };
    } catch (err) {
      last = { status: 504, body: rpcError(null, -32603, `upstream: ${redact(err)}`) };
    } finally {
      clearTimeout(timer);
    }
  }
  return last;
}

// -------------------------------------------------------------------- server

function readBody(req: http.IncomingMessage, limit = MAX_BODY_BYTES): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on('data', (chunk: Buffer) => {
      size += chunk.length;
      if (size > limit) {
        reject(new Error('body too large'));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

const jsonError = (message: string) => JSON.stringify({ error: message });

/**
 * Accept one signed chat message, or say exactly why not.
 *
 * Order matters: parse, then shape, then signature, then freshness. The
 * signature check is the expensive one and the only one that cannot be fooled,
 * so cheap malformed input never reaches it, and nothing is written until it
 * passes. The row is rebuilt from validated fields rather than forwarded as
 * received, so extra keys a caller invents cannot reach the table.
 */
async function handleChat(req: http.IncomingMessage, res: http.ServerResponse, origin?: string) {
  if (!CHAT_ENABLED) {
    send(res, origin, 503, jsonError('chat is not configured'));
    return;
  }

  let raw: string;
  try {
    raw = await readBody(req, CHAT_MAX_BODY_BYTES);
  } catch (err) {
    send(res, origin, 413, jsonError(redact(err)));
    return;
  }

  let input: any;
  try {
    input = JSON.parse(raw);
  } catch {
    send(res, origin, 400, jsonError('parse error'));
    return;
  }

  const starId = Number(input?.star_id ?? input?.starId);
  const wallet = String(input?.wallet ?? '');
  const body = normalizeChatBody(input?.body);
  const ts = canonicalTs(input?.ts);
  const signature = String(input?.signature ?? '');

  if (!Number.isInteger(starId) || starId <= 0) {
    send(res, origin, 400, jsonError('bad star'));
    return;
  }
  if (!body) {
    send(res, origin, 400, jsonError('empty message'));
    return;
  }
  if (String(input?.body ?? '').length > CHAT_MAX_BODY) {
    send(res, origin, 400, jsonError(`message over ${CHAT_MAX_BODY} characters`));
    return;
  }
  if (!ts) {
    send(res, origin, 400, jsonError('bad timestamp'));
    return;
  }

  const row = { cluster: CHAT_CLUSTER, star_id: starId, wallet, body, ts, signature };

  if (!verifyChatRow(row)) {
    send(res, origin, 401, jsonError('signature does not match that wallet'));
    return;
  }
  // A signature stays valid forever, so without this a captured message could be
  // replayed at any point in the future. The unique index on (cluster,
  // signature) stops an immediate re-post; this stops a hoarded one.
  if (!chatRowFresh(row)) {
    send(res, origin, 401, jsonError('message is too old'));
    return;
  }

  try {
    const upstream = await fetch(`${SUPABASE_URL}/rest/v1/corium_chat`, {
      method: 'POST',
      headers: {
        apikey: SUPABASE_SERVICE_KEY,
        Authorization: `Bearer ${SUPABASE_SERVICE_KEY}`,
        'Content-Type': 'application/json',
        Prefer: 'return=representation',
      },
      body: JSON.stringify(row),
      signal: AbortSignal.timeout(8_000),
    });
    const text = await upstream.text();
    if (upstream.status === 409) {
      send(res, origin, 409, jsonError('already sent'));
      return;
    }
    if (!upstream.ok) {
      // Never hand a client the database's own words: they carry table and
      // policy names, and the rate-limit trigger raises a bare exception.
      console.warn(`[rpc-proxy] chat insert ${upstream.status}: ${text.slice(0, 200)}`);
      const tooFast = /rate limit/i.test(text);
      send(
        res,
        origin,
        tooFast ? 429 : 502,
        jsonError(tooFast ? 'slow down' : 'could not save message'),
        tooFast ? { 'Retry-After': '2' } : {}
      );
      return;
    }
    send(res, origin, 200, text);
  } catch (err) {
    console.warn('[rpc-proxy] chat insert failed', redact(err));
    send(res, origin, 502, jsonError('could not save message'));
  }
}

const server = http.createServer(async (req, res) => {
  const origin = req.headers.origin;
  const url = req.url ?? '/';

  if (req.method === 'OPTIONS') {
    res.writeHead(204, corsHeaders(origin));
    res.end();
    return;
  }

  if (req.method === 'GET' && (url === '/health' || url === '/healthz')) {
    send(
      res,
      origin,
      200,
      JSON.stringify({
        ok: true,
        upstream: upstreamHost(),
        websocket: true,
        chat: CHAT_ENABLED,
        cluster: CHAT_CLUSTER,
        uptimeSec: Math.round(process.uptime()),
      })
    );
    return;
  }

  if (req.method !== 'POST') {
    send(res, origin, 405, rpcError(null, -32600, 'POST JSON-RPC to / or a message to /chat'));
    return;
  }

  if (!originAllowed(origin)) {
    console.warn(`[rpc-proxy] blocked origin ${origin}`);
    send(res, origin, 403, rpcError(null, -32600, 'origin not allowed'));
    return;
  }

  const ip = clientIp(req);
  if (!takeToken(ip)) {
    send(res, origin, 429, rpcError(null, -32005, 'rate limit: slow down'), {
      'Retry-After': '1',
    });
    return;
  }

  // Behind the origin allowlist and the per-IP bucket, in front of the JSON-RPC
  // method allowlist - a chat post is not a JSON-RPC call and must not be judged
  // as one, but it should still pay for a token.
  if (url === '/chat') {
    await handleChat(req, res, origin);
    return;
  }

  let raw: string;
  try {
    raw = await readBody(req);
  } catch (err) {
    send(res, origin, 413, rpcError(null, -32600, redact(err)));
    return;
  }

  let payload: unknown;
  try {
    payload = JSON.parse(raw);
  } catch {
    send(res, origin, 400, rpcError(null, -32700, 'parse error'));
    return;
  }

  const reason = rejectPayload(payload);
  if (reason) {
    const id = Array.isArray(payload) ? null : (payload as RpcCall)?.id;
    console.warn(`[rpc-proxy] rejected from ${ip}: ${reason}`);
    send(res, origin, 400, rpcError(id, -32601, reason));
    return;
  }

  const result = await forward(raw);
  send(
    res,
    origin,
    result.status,
    result.body,
    result.retryAfter ? { 'Retry-After': result.retryAfter } : {}
  );
});

// ----------------------------------------------------------------- websocket

const wss = new WebSocketServer({ noServer: true });
const wsCounts = new Map<string, number>();

server.on('upgrade', (req, socket, head) => {
  if (!originAllowed(req.headers.origin)) {
    socket.write('HTTP/1.1 403 Forbidden\r\n\r\n');
    socket.destroy();
    return;
  }
  const ip = clientIp(req);
  if ((wsCounts.get(ip) ?? 0) >= MAX_WS_PER_IP) {
    socket.write('HTTP/1.1 429 Too Many Requests\r\n\r\n');
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket as any, head, (client) => bridge(client, ip));
});

function bridge(client: WebSocket, ip: string) {
  wsCounts.set(ip, (wsCounts.get(ip) ?? 0) + 1);

  const upstream = new WebSocket(UPSTREAM_WS);
  /** Subscriptions are sent the instant the socket opens, before upstream is ready. */
  const backlog: string[] = [];
  let closed = false;

  const shutdown = () => {
    if (closed) return;
    closed = true;
    wsCounts.set(ip, Math.max(0, (wsCounts.get(ip) ?? 1) - 1));
    if (wsCounts.get(ip) === 0) wsCounts.delete(ip);
    try {
      client.close();
    } catch {}
    try {
      upstream.close();
    } catch {}
  };

  upstream.on('open', () => {
    for (const msg of backlog) upstream.send(msg);
    backlog.length = 0;
  });
  upstream.on('message', (data) => {
    if (client.readyState === WebSocket.OPEN) client.send(data.toString());
  });
  upstream.on('error', (err) => {
    console.warn(`[rpc-proxy] upstream ws error: ${redact(err)}`);
    shutdown();
  });
  upstream.on('close', shutdown);

  client.on('message', (data) => {
    const text = data.toString();
    let call: RpcCall;
    try {
      call = JSON.parse(text);
    } catch {
      client.send(rpcError(null, -32700, 'parse error'));
      return;
    }
    if (typeof call?.method !== 'string' || !WS_METHODS.has(call.method)) {
      client.send(rpcError(call?.id, -32601, `method not allowed: ${String(call?.method)}`));
      return;
    }
    if (upstream.readyState === WebSocket.OPEN) upstream.send(text);
    else if (upstream.readyState === WebSocket.CONNECTING) backlog.push(text);
    else shutdown();
  });
  client.on('error', shutdown);
  client.on('close', shutdown);
}

/** Helius drops idle sockets after 10 minutes; a ping keeps the bridge warm. */
setInterval(() => {
  for (const client of wss.clients) {
    if (client.readyState === WebSocket.OPEN) client.ping();
  }
}, 30_000).unref();

// -------------------------------------------------------------------- listen

server.listen(PORT, () => {
  console.log(
    `[rpc-proxy] :${PORT} -> ${upstreamHost()} ` +
      `cluster=${CHAT_CLUSTER} ` +
      `(origins: ${ALLOWED_ORIGINS.join(', ') || 'any'}, ` +
      `burst: ${RATE_BURST}, refill: ${RATE_REFILL_PER_SEC}/s)`
  );
});

for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.on(signal, () => {
    console.log(`[rpc-proxy] ${signal}, closing`);
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 5_000).unref();
  });
}
