/**
 * Measure ORAO request → fulfill wall times from real txs.
 * Also measure soldust request_push_vrf → resolve_push (includes crank).
 */
import { Connection, PublicKey } from '@solana/web3.js';

const ORAO = new PublicKey('VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y');
const SOLDUDST = new PublicKey('CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S');

/**
 * This walks thousands of signatures, so it wants a paid node on both
 * clusters. Set HELIUS_KEY, or point the two URLs anywhere you like.
 */
const KEY = process.env.HELIUS_KEY;
const DEVNET_RPC =
  process.env.DEVNET_RPC_URL ||
  (KEY ? `https://devnet.helius-rpc.com/?api-key=${KEY}` : 'https://api.devnet.solana.com');
const MAINNET_RPC =
  process.env.MAINNET_RPC_URL ||
  (KEY ? `https://mainnet.helius-rpc.com/?api-key=${KEY}` : '');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function stats(xs) {
  const a = xs.filter((n) => Number.isFinite(n)).sort((x, y) => x - y);
  if (!a.length) return null;
  const p = (q) => a[Math.min(a.length - 1, Math.floor(q * (a.length - 1)))];
  const mean = a.reduce((s, n) => s + n, 0) / a.length;
  return {
    n: a.length,
    min: a[0],
    p25: p(0.25),
    p50: p(0.5),
    p75: p(0.75),
    p90: p(0.9),
    max: a[a.length - 1],
    mean: Math.round(mean * 10) / 10,
  };
}

function fmt(s) {
  if (!s) return 'no samples';
  return `n=${s.n}  min ${s.min}s  p25 ${s.p25}s  median ${s.p50}s  p75 ${s.p75}s  p90 ${s.p90}s  max ${s.max}s  mean ${s.mean}s`;
}

async function getTx(connection, signature) {
  return connection.getTransaction(signature, {
    maxSupportedTransactionVersion: 0,
    commitment: 'confirmed',
  });
}

async function pairRandomness(connection, randomness, requestSig, requestTime) {
  const sigs = await connection.getSignaturesForAddress(randomness, { limit: 12 });
  const others = sigs.filter((s) => s.signature !== requestSig && !s.err);
  for (const s of others) {
    const tx = await getTx(connection, s.signature);
    if (!tx) continue;
    const keys = tx.transaction.message.getAccountKeys().staticAccountKeys;
    if (!keys.some((k) => k.equals(ORAO))) continue;
    const t = s.blockTime ?? tx.blockTime;
    if (t == null || requestTime == null) continue;
    const dt = t - requestTime;
    if (dt < 0 || dt > 600) continue;
    return { fulfillSig: s.signature, dt, fulfillTime: t };
  }
  return null;
}

async function measureSoldust(connection, limit = 80) {
  const sigs = await connection.getSignaturesForAddress(SOLDUDST, { limit });
  const oraoWait = [];
  const crankWait = [];
  let requests = 0;
  for (const s of sigs) {
    if (s.err) continue;
    const tx = await getTx(connection, s.signature);
    if (!tx?.meta) continue;
    const logs = tx.meta.logMessages ?? [];
    const isReq = logs.some((l) => /RequestPushVrf|request_push_vrf/i.test(l));
    if (!isReq) continue;
    requests++;
    const keys = tx.transaction.message.getAccountKeys().staticAccountKeys;
    const pre = tx.meta.preBalances;
    const post = tx.meta.postBalances;
    let randomness = null;
    keys.forEach((k, i) => {
      if (pre[i] === 0 && post[i] > 0 && !k.equals(SOLDUDST)) randomness = k;
    });
    if (!randomness) continue;
    const reqTime = s.blockTime ?? tx.blockTime;
    const pair = await pairRandomness(connection, randomness, s.signature, reqTime);
    if (pair) {
      oraoWait.push(pair.dt);
      if (oraoWait.length <= 24) {
        console.log(`  ${pair.dt.toString().padStart(3)}s  req ${s.signature.slice(0, 8)}…  fulfill ${pair.fulfillSig.slice(0, 8)}…`);
      }
    }

    const rsigs = await connection.getSignaturesForAddress(randomness, { limit: 12 });
    for (const rs of rsigs) {
      if (rs.signature === s.signature || rs.err) continue;
      const rtx = await getTx(connection, rs.signature);
      if (!rtx?.meta) continue;
      const rlogs = rtx.meta.logMessages ?? [];
      if (!rlogs.some((l) => /ResolvePush|resolve_push/i.test(l))) continue;
      const rt = rs.blockTime ?? rtx.blockTime;
      if (rt != null && reqTime != null) {
        const dt = rt - reqTime;
        if (dt >= 0 && dt < 600) crankWait.push(dt);
      }
      break;
    }
    await sleep(40);
  }
  return { requests, oraoWait, crankWait };
}

async function measureOraoProgram(connection, limit = 80) {
  const sigs = await connection.getSignaturesForAddress(ORAO, { limit });
  const waits = [];
  const seen = new Set();
  for (const s of sigs) {
    if (s.err) continue;
    const tx = await getTx(connection, s.signature);
    if (!tx?.meta) continue;
    const logs = tx.meta.logMessages ?? [];
    const isFulfill = logs.some((l) => /Instruction: Fulfill/i.test(l) || /fulfill_v2/i.test(l));
    if (!isFulfill) continue;
    const keys = tx.transaction.message.getAccountKeys().staticAccountKeys;
    for (const k of keys) {
      if (k.equals(ORAO) || seen.has(k.toBase58())) continue;
      const hist = await connection.getSignaturesForAddress(k, { limit: 6 });
      const ok = hist.filter((h) => !h.err && h.blockTime != null);
      // Fresh randomness accounts are request + fulfill. Skip treasuries / config.
      if (ok.length !== 2) continue;
      const times = ok.map((h) => h.blockTime).sort((a, b) => a - b);
      const dt = times[1] - times[0];
      if (dt >= 0 && dt < 300) {
        waits.push(dt);
        seen.add(k.toBase58());
        break;
      }
    }
    await sleep(30);
    if (waits.length >= 30) break;
  }
  return waits;
}

async function run() {
  if (!KEY && !process.env.DEVNET_RPC_URL) {
    console.log('warning: no HELIUS_KEY - the public node will rate-limit this scan\n');
  }
  const devnet = new Connection(DEVNET_RPC, 'confirmed');
  const mainnet = MAINNET_RPC ? new Connection(MAINNET_RPC, 'confirmed') : null;

  console.log('=== soldust DEVNET (your program) ===');
  try {
    const s = await measureSoldust(devnet, 100);
    console.log(`request_push_vrf txs scanned: ${s.requests}`);
    console.log(`ORAO fulfill after request_push_vrf:  ${fmt(stats(s.oraoWait))}`);
    console.log(`resolve_push after request_push_vrf:  ${fmt(stats(s.crankWait))}`);
  } catch (e) {
    console.log('soldust devnet failed:', e.message);
  }

  console.log('\n=== ORAO program DEVNET (all callers) ===');
  try {
    const w = await measureOraoProgram(devnet, 80);
    console.log(fmt(stats(w)));
  } catch (e) {
    console.log('orao devnet failed:', e.message);
  }

  console.log('\n=== ORAO program MAINNET (all callers) ===');
  if (!mainnet) {
    console.log('skipped: set HELIUS_KEY or MAINNET_RPC_URL');
    return;
  }
  try {
    const w = await measureOraoProgram(mainnet, 80);
    console.log(fmt(stats(w)));
  } catch (e) {
    console.log('orao mainnet failed:', e.message);
  }
}

run().catch((e) => {
  console.error(e);
  process.exit(1);
});
