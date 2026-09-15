/**
 * Catch up program logs into Supabase. The live page does not do this.
 *
 * Uses the same Helius URL as the crank (onchain/.env). Incremental after
 * the first full walk.
 */

import { EventParser } from '@coral-xyz/anchor';

import { historyCluster } from '../../shared/chain/config.js';
import { eventKind, historyWallet } from '../../shared/chain/playerKind.js';
import { closePushFromTx, findClosePushTx } from '../../shared/chain/rents.js';

import { context } from './lib';

const PAGE = 100;
const TX_CHUNK = 20;

function camel(name: string) {
  if (!name) return '';
  return name.charAt(0).toLowerCase() + name.slice(1);
}

function jsonSafe(value: any): any {
  if (value == null || typeof value === 'boolean' || typeof value === 'string') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : 0;
  if (typeof value === 'bigint') return value.toString();
  if (typeof value.toBase58 === 'function') return value.toBase58();
  if (Buffer.isBuffer(value) || ArrayBuffer.isView(value)) {
    return Buffer.from(value as Uint8Array).toString('hex');
  }
  if (Array.isArray(value)) {
    if (value.length && value.every((n) => typeof n === 'number' && n >= 0 && n <= 255)) {
      return Buffer.from(value).toString('hex');
    }
    return value.map(jsonSafe);
  }
  if (typeof value === 'object') {
    if (typeof value.toString === 'function' && (value.constructor?.name === 'BN' || Array.isArray(value.words))) {
      return value.toString();
    }
    const out: Record<string, any> = {};
    for (const [k, v] of Object.entries(value)) out[k] = jsonSafe(v);
    return out;
  }
  return String(value);
}

function cluster() {
  return historyCluster();
}

function eventId(ev: { signature: string; name: string; data: any }) {
  const d = ev.data ?? {};
  return `${cluster()}:${ev.signature}:${ev.name}:${d.push ?? d.pushId ?? d.starId ?? ''}`;
}

function starIdOf(data: any): string | null {
  const v = data?.starId ?? data?.star_id;
  if (v == null || v === '') return null;
  return String(v);
}

function sleep(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}

function isRateLimited(err: any) {
  return /429|rate|too many|busy|limit/i.test(String(err?.message ?? err ?? ''));
}

async function withRetry<T>(fn: () => Promise<T>, tries = 5): Promise<T> {
  let last: any;
  for (let i = 0; i < tries; i++) {
    try {
      return await fn();
    } catch (err) {
      last = err;
      if (i === tries - 1 || !isRateLimited(err)) throw err;
      await sleep(350 * 2 ** i);
    }
  }
  throw last;
}

function supabaseEnv() {
  const url = (process.env.SUPABASE_URL || process.env.VITE_SUPABASE_URL || '').replace(/\/$/, '');
  const key = process.env.SUPABASE_SERVICE_ROLE_KEY || '';
  if (!url || !key) return null;
  return { url, key };
}

export function historyEnabled() {
  return !!supabaseEnv();
}

async function sbFetch(path: string, init: RequestInit = {}) {
  const env = supabaseEnv();
  if (!env) throw new Error('SUPABASE_URL / SUPABASE_SERVICE_ROLE_KEY missing');
  const method = (init.method || 'GET').toUpperCase();
  const res = await fetch(`${env.url}/rest/v1${path}`, {
    ...init,
    headers: {
      apikey: env.key,
      Authorization: `Bearer ${env.key}`,
      Accept: 'application/json',
      'Content-Type': 'application/json',
      Prefer: method === 'GET' ? 'return=representation' : 'resolution=merge-duplicates,return=minimal',
      ...(init.headers || {}),
    },
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`supabase ${res.status} ${body.slice(0, 240)}`);
  }
  const text = await res.text();
  return text ? JSON.parse(text) : null;
}

async function loadCursor(programId: string) {
  const rows = await sbFetch(
    `/soldust_cursor?program_id=eq.${encodeURIComponent(programId)}` +
      `&cluster=eq.${encodeURIComponent(cluster())}&select=*&limit=1`
  );
  return Array.isArray(rows) && rows[0] ? rows[0] : null;
}

async function saveCursor(programId: string, patch: Record<string, any>) {
  await sbFetch('/soldust_cursor', {
    method: 'POST',
    headers: { Prefer: 'resolution=merge-duplicates,return=minimal' },
    body: JSON.stringify({
      program_id: programId,
      oldest_complete: false,
      tip_slot: 0,
      oldest_signature: null,
      ...patch,
      cluster: cluster(),
      updated_at: new Date().toISOString(),
    }),
  });
}

async function upsertEvents(rows: any[]) {
  if (!rows.length) return;
  const chunk = 200;
  for (let i = 0; i < rows.length; i += chunk) {
    await sbFetch('/soldust_events', {
      method: 'POST',
      headers: { Prefer: 'resolution=merge-duplicates,return=minimal' },
      body: JSON.stringify(rows.slice(i, i + chunk)),
    });
  }
}

export function encodeEvent(programId: string, name: string, data: any, meta: {
  signature: string;
  slot?: number;
  blockTime?: number;
}) {
  const payload = jsonSafe(data) ?? {};
  const ev = {
    name: camel(name),
    signature: meta.signature || '',
    slot: meta.slot ?? 0,
    blockTime: meta.blockTime ?? 0,
    data: payload,
  };
  const kind = eventKind(ev.name, payload) || null;
  const wallet = historyWallet(ev.name, payload) || null;
  return {
    id: eventId(ev),
    program_id: programId,
    cluster: cluster(),
    signature: ev.signature,
    slot: ev.slot,
    block_time: ev.blockTime,
    name: ev.name,
    star_id: starIdOf(payload),
    wallet,
    kind,
    data: payload,
  };
}

export async function ingestEncoded(rows: any[]) {
  await upsertEvents(rows);
}

async function signaturesPage(
  connection: ReturnType<typeof context>['connection'],
  programId: ReturnType<typeof context>['programId'],
  before?: string
) {
  return withRetry(() =>
    connection.getSignaturesForAddress(programId, { limit: PAGE, before })
  );
}

async function parseAndStore(
  ctx: ReturnType<typeof context>,
  parser: EventParser,
  infos: { signature: string; slot: number; blockTime: number | null }[]
) {
  const programId = ctx.programId.toBase58();
  const chronological = infos.slice().reverse();
  const rows: any[] = [];
  const closes: { push: string; signature: string; lamports: number }[] = [];
  for (let i = 0; i < chronological.length; i += TX_CHUNK) {
    const chunk = chronological.slice(i, i + TX_CHUNK);
    const txs = await withRetry(() =>
      ctx.connection.getTransactions(
        chunk.map((s) => s.signature),
        { commitment: 'confirmed', maxSupportedTransactionVersion: 1 }
      )
    );
    for (let j = 0; j < txs.length; j++) {
      const tx = txs[j];
      const info = chunk[j];
      if (!tx?.meta) continue;
      if (tx.meta.logMessages) {
        try {
          for (const event of parser.parseLogs(tx.meta.logMessages)) {
            rows.push(
              encodeEvent(programId, event.name, event.data, {
                signature: info.signature,
                slot: info.slot,
                blockTime: tx.blockTime ?? info.blockTime ?? 0,
              })
            );
          }
        } catch {
          /* skip undecodable logs */
        }
      }
      const closed = closePushFromTx(programId, tx);
      if (closed) {
        rows.push(
          encodeEvent(
            programId,
            'pushClosed',
            {
              player: closed.player,
              push: closed.push,
              amount: closed.lamports,
            },
            {
              signature: info.signature,
              slot: info.slot,
              blockTime: tx.blockTime ?? info.blockTime ?? 0,
            }
          )
        );
        closes.push({
          push: closed.push,
          signature: info.signature,
          lamports: closed.lamports,
        });
      }
    }
    if (i + TX_CHUNK < chronological.length) await sleep(40);
  }
  await upsertEvents(rows);
  for (const c of closes) {
    try {
      await attachRentToSettle(programId, c.push, {
        closeSignature: c.signature,
        closeLamports: c.lamports,
      });
    } catch {
      /* settle row may land on a later page */
    }
  }
  return rows.length;
}

async function patchEventData(id: string, data: any) {
  await sbFetch(`/soldust_events?id=eq.${encodeURIComponent(id)}`, {
    method: 'PATCH',
    headers: { Prefer: 'return=minimal' },
    body: JSON.stringify({ data }),
  });
}

/** Merge rent fields onto the settle row so YOU can read them without a join. */
export async function attachRentToSettle(
  programId: string,
  push: string,
  patch: Record<string, any>
) {
  if (!historyEnabled() || !push) return 0;
  const pid = encodeURIComponent(programId);
  const pk = encodeURIComponent(push);
  const rows = await sbFetch(
    `/soldust_events?program_id=eq.${pid}&cluster=eq.${encodeURIComponent(cluster())}` +
      `&or=(name.eq.pushResolved,name.eq.pushCancelled)&data->>push=eq.${pk}&select=id,data&limit=8`
  );
  if (!Array.isArray(rows) || !rows.length) return 0;
  let n = 0;
  for (const row of rows) {
    const prev = row.data || {};
    let changed = false;
    for (const [k, v] of Object.entries(patch)) {
      if (v == null || v === '') continue;
      if (String(prev[k] ?? '') !== String(v)) changed = true;
    }
    if (!changed) continue;
    await patchEventData(row.id, { ...prev, ...patch });
    n += 1;
  }
  return n;
}

export async function recordPushClosed(
  programId: string,
  meta: {
    signature: string;
    slot?: number;
    blockTime?: number;
    push: string;
    player?: string;
    starId?: string | number;
    pushId?: string | number;
    lamports?: number;
    randomness?: string;
  }
) {
  if (!historyEnabled()) return;
  const payload = {
    player: meta.player || '',
    starId: meta.starId != null ? String(meta.starId) : undefined,
    pushId: meta.pushId != null ? String(meta.pushId) : undefined,
    push: meta.push,
    amount: meta.lamports ?? 0,
    randomness: meta.randomness || undefined,
  };
  await ingestEncoded([
    encodeEvent(programId, 'pushClosed', payload, {
      signature: meta.signature,
      slot: meta.slot ?? 0,
      blockTime: meta.blockTime ?? Math.floor(Date.now() / 1000),
    }),
  ]);
  await attachRentToSettle(programId, meta.push, {
    closeSignature: meta.signature,
    closeLamports: meta.lamports ?? 0,
    ...(meta.randomness ? { randomness: meta.randomness } : {}),
  });
}

/**
 * Fill in `close_push` signatures on last-hits. Default is a small crank tick.
 *
 * There is no oracle rent to chase: the player prepays a fixed budget that the
 * program either hands to the crank or returns inside the refund, so nothing
 * about it has to be reconciled from chain history.
 */
export async function backfillRents(
  ctx: ReturnType<typeof context>,
  { limit = 12, offset = 0 }: { limit?: number; offset?: number } = {}
) {
  if (!historyEnabled()) return { closed: 0, scanned: 0, done: true };
  const programId = ctx.programId.toBase58();
  const pid = encodeURIComponent(programId);
  const start = Math.max(0, Math.floor(Number(offset) || 0));
  const page = Math.max(1, Math.floor(Number(limit) || 12));
  let closed = 0;

  const lastHits = await sbFetch(
    `/soldust_events?program_id=eq.${pid}&cluster=eq.${encodeURIComponent(cluster())}` +
      `&or=(kind.eq.push,kind.eq.nova,kind.eq.horizon,kind.eq.refund)` +
      `&select=id,data,wallet&order=slot.desc&limit=${page}&offset=${start}`
  );
  const rows = Array.isArray(lastHits) ? lastHits : [];
  for (const row of rows) {
    const d = row.data || {};
    const push = d.push;
    if (!push) continue;
    if (d.closeSignature || d.close_signature) continue;
    const found = await findClosePushTx(ctx.connection, programId, push);
    if (found) {
      await recordPushClosed(programId, {
        signature: found.signature,
        push,
        player: found.player || d.player || row.wallet,
        lamports: found.lamports,
      });
      closed += 1;
    }
  }

  return { closed, scanned: rows.length, done: rows.length < page };
}

/**
 * New signatures first (so live history is complete), then walk older pages
 * until genesis. Tip is the newest sig we have; oldest_signature is the
 * backfill cursor. Never mark complete after a capped page.
 */
export async function catchUpHistory(
  ctx: ReturnType<typeof context>,
  { maxPages = 6, onProgress }: { maxPages?: number; onProgress?: (p: any) => void } = {}
) {
  if (!historyEnabled()) return { added: 0, skipped: true };
  const programId = ctx.programId.toBase58();
  const parser = new EventParser(ctx.programId, ctx.program.coder);
  const cursor = (await loadCursor(programId)) ?? {};
  let tip = cursor.tip_signature as string | undefined;
  let oldestSig = cursor.oldest_signature as string | undefined;
  let oldestComplete = !!cursor.oldest_complete;
  let stored = 0;
  let fetched = 0;

  const newestPage = await signaturesPage(ctx.connection, ctx.programId);
  if (!newestPage.length) {
    await saveCursor(programId, {
      oldest_complete: true,
      tip_signature: tip || null,
      oldest_signature: oldestSig || null,
      tip_slot: cursor.tip_slot ?? 0,
    });
    return { added: 0, stored: 0, oldestComplete: true };
  }

  const fresh: { signature: string; slot: number; blockTime: number | null }[] = [];
  for (const s of newestPage) {
    if (tip && s.signature === tip) break;
    fresh.push({ signature: s.signature, slot: s.slot, blockTime: s.blockTime ?? null });
  }
  // If the tip is not on this page, keep paging until we hit it (or cap).
  let before = newestPage[newestPage.length - 1]?.signature;
  for (let page = 1; page < maxPages && tip && fresh.length && fresh.length % PAGE === 0; page++) {
    const batch = await signaturesPage(ctx.connection, ctx.programId, before);
    if (!batch.length) break;
    let hit = false;
    for (const s of batch) {
      if (s.signature === tip) {
        hit = true;
        break;
      }
      fresh.push({ signature: s.signature, slot: s.slot, blockTime: s.blockTime ?? null });
    }
    if (hit) break;
    before = batch[batch.length - 1].signature;
  }

  if (fresh.length) {
    fetched += fresh.length;
    stored += await parseAndStore(ctx, parser, fresh);
    tip = fresh[0].signature;
    if (!oldestSig) oldestSig = fresh[fresh.length - 1].signature;
    onProgress?.({ phase: 'new', fetched: fresh.length, stored });
  }

  if (!oldestComplete) {
    let walkFrom = oldestSig || newestPage[newestPage.length - 1].signature;
    for (let page = 0; page < maxPages; page++) {
      const batch = await signaturesPage(ctx.connection, ctx.programId, walkFrom);
      if (!batch.length) {
        oldestComplete = true;
        break;
      }
      const older = batch.map((s) => ({
        signature: s.signature,
        slot: s.slot,
        blockTime: s.blockTime ?? null,
      }));
      fetched += older.length;
      stored += await parseAndStore(ctx, parser, older);
      walkFrom = older[older.length - 1].signature;
      oldestSig = walkFrom;
      onProgress?.({ phase: 'old', fetched, stored, page });
      if (batch.length < PAGE) {
        oldestComplete = true;
        break;
      }
    }
  }

  await saveCursor(programId, {
    oldest_complete: oldestComplete,
    tip_signature: tip || null,
    oldest_signature: oldestSig || null,
    tip_slot: newestPage[0]?.slot ?? 0,
  });
  return { added: fetched, stored, oldestComplete };
}
