/**
 * Dump one star's on-chain book: star, rounds, leftover pushes, vault vs liabilities.
 *
 *   yarn tsx scripts/audit-star.ts --star 2
 */

import { context, enumName, parseArgs, sol, starPda } from './lib';
import { historyCluster } from '../../shared/chain/config.js';

function n(v: any) {
  return Number(v?.toString?.() ?? v ?? 0);
}

async function main() {
  const args = parseArgs();
  const starId = BigInt(String(args.star ?? 2));
  const { program, programId, connection, config, vault } = context(args);

  const starPk = starPda(programId, starId);
  const star = await program.account.star.fetch(starPk).catch(() => null);
  const cfg = await program.account.config.fetch(config);
  const vaultLamports = await connection.getBalance(vault);

  console.log(`== star #${starId} ${starPk.toBase58()} ==`);
  if (!star) {
    console.log('missing');
    return;
  }
  console.log(`status            ${enumName(star.status)}`);
  console.log(`mass              ${sol(star.totalMass)}`);
  console.log(`prize             ${sol(star.prizePool)}`);
  console.log(`pending pushes    ${star.pendingPushes} / ${sol(star.pendingLamports)}`);
  console.log(`push counter      ${star.pushCounter}`);
  console.log(`settle cursor     ${star.settleCursor}`);
  console.log(`current round     ${star.currentRound}`);
  console.log(`killer            ${star.killer?.toBase58?.() ?? star.killer}`);

  console.log('\n== config / vault ==');
  console.log(`current star      #${cfg.currentStarId}`);
  console.log(`vault             ${sol(vaultLamports)}`);
  console.log(`pending liab      ${sol(cfg.pendingLiability)}`);
  console.log(`prize liab        ${sol(cfg.prizeLiability)}`);
  console.log(`next reserve      ${sol(cfg.nextStarReserve)}`);
  console.log(`protocol          ${sol(cfg.protocolAccrued)}`);
  const reserved = n(cfg.pendingLiability) + n(cfg.prizeLiability) + n(cfg.nextStarReserve);
  console.log(`reserved          ${sol(reserved)}`);
  console.log(`free              ${sol(vaultLamports - reserved)}`);

  const rounds = (
    await program.account.round.all([{ dataSize: program.account.round.size }])
  ).filter((r) => r.account.starId.toString() === starId.toString());
  rounds.sort((a, b) => n(a.account.roundId) - n(b.account.roundId));
  console.log(`\n== rounds (${rounds.length}) ==`);
  let stakeOpen = 0;
  for (const r of rounds) {
    const a = r.account;
    const st = enumName(a.status);
    console.log(
      `  #${a.roundId}  ${st.padEnd(10)}  members ${a.memberCount}  stake ${sol(a.stake)}  ` +
        `first ${a.firstPushId}`
    );
    if (st === 'open' || st === 'requested') stakeOpen += n(a.stake);
  }

  const pushes = (await program.account.pendingPush.all()).filter(
    (p) => p.account.starId.toString() === starId.toString()
  );
  pushes.sort((a, b) => n(a.account.pushId) - n(b.account.pushId));
  const byStatus: Record<string, { n: number; sol: number; mine?: number }> = {};
  const players = new Map<string, { pending: number; settled: number; cancelled: number; amt: number }>();
  console.log(`\n== leftover push accounts (${pushes.length}) ==`);
  for (const p of pushes) {
    const a = p.account;
    const st = enumName(a.status);
    byStatus[st] = byStatus[st] || { n: 0, sol: 0 };
    byStatus[st].n++;
    byStatus[st].sol += n(a.amount);
    const pk = a.player.toBase58();
    const row = players.get(pk) || { pending: 0, settled: 0, cancelled: 0, amt: 0 };
    if (st === 'pending') row.pending += n(a.amount);
    else if (st === 'cancelled') row.cancelled += n(a.amount);
    else row.settled += n(a.amount);
    row.amt += n(a.amount);
    players.set(pk, row);
    if (st === 'pending') {
      console.log(
        `  PENDING push #${a.pushId} round #${a.roundId} ${sol(a.amount)} ${pk.slice(0, 8)}…`
      );
    }
  }
  for (const [st, v] of Object.entries(byStatus)) {
    console.log(`  ${st}: ${v.n} accounts, ${sol(v.sol)}`);
  }

  const cursor = n(star.settleCursor);
  const counter = n(star.pushCounter);
  console.log(`\n== queue ==`);
  console.log(`settle_cursor ${cursor} / push_counter ${counter}  leftover ids ${counter - cursor}`);
  if (cursor < counter) {
    console.log('WARNING: cursor has not caught the counter - something is still in line');
  } else {
    console.log('cursor caught up: every assigned push_id was processed');
  }

  console.log('\n== players still holding a push account on this star ==');
  for (const [pk, v] of [...players.entries()].sort((a, b) => b[1].amt - a[1].amt)) {
    console.log(
      `  ${pk}  pending ${sol(v.pending)}  settled-unclosed ${sol(v.settled)}  cancelled-unclosed ${sol(v.cancelled)}`
    );
  }

  const base = (process.env.SUPABASE_URL || process.env.VITE_SUPABASE_URL || '').replace(/\/$/, '');
  const key = process.env.SUPABASE_SERVICE_ROLE_KEY || process.env.VITE_SUPABASE_ANON_KEY || '';
  if (!base || !key) return;
  const pid = programId.toBase58();
  const url =
    `${base}/rest/v1/soldust_events?program_id=eq.${encodeURIComponent(pid)}` +
    `&cluster=eq.${encodeURIComponent(historyCluster())}` +
    `&star_id=eq.${starId}` +
    `&select=name,signature,slot,kind,data&order=slot.asc,signature.asc&limit=1000`;
  const res = await fetch(url, {
    headers: { apikey: key, Authorization: `Bearer ${key}`, Accept: 'application/json' },
  });
  if (!res.ok) {
    console.log(`\n== history == ${res.status} ${await res.text()}`);
    return;
  }
  const rows = (await res.json()) as any[];
  const tally: Record<string, number> = {};
  let resolvedAmt = 0;
  let cancelledAmt = 0;
  let cancelledN = 0;
  let resolvedN = 0;
  let killed = 0;
  const byPlayer = new Map<string, { resolved: number; cancelled: number; rn: number; cn: number }>();
  for (const row of rows) {
    tally[row.name] = (tally[row.name] || 0) + 1;
    const d = row.data || {};
    const player = String(d.player || d.wallet || '');
    const amt = n(d.amount);
    const rec = byPlayer.get(player) || { resolved: 0, cancelled: 0, rn: 0, cn: 0 };
    if (row.name === 'pushResolved' || row.name === 'PushResolved') {
      resolvedAmt += amt;
      resolvedN++;
      rec.resolved += amt;
      rec.rn++;
      if (d.survived === false) killed++;
    }
    if (row.name === 'pushCancelled' || row.name === 'PushCancelled') {
      cancelledAmt += amt;
      cancelledN++;
      rec.cancelled += amt;
      rec.cn++;
    }
    byPlayer.set(player, rec);
  }
  console.log(`\n== hosted history detail ==`);
  for (const row of rows) {
    if (row.name !== 'pushResolved' && row.name !== 'pushCancelled' && row.name !== 'pushRequested' && row.name !== 'starDestroyed' && row.name !== 'prizeClaimed') {
      continue;
    }
    const d = row.data || {};
    console.log(
      `  ${row.name.padEnd(14)} push ${d.pushId ?? d.push_id ?? '-'} round ${d.roundId ?? d.round_id ?? '-'} ` +
        `${sol(d.amount ?? 0)} surv=${d.survived} void=${d.roundVoided ?? d.round_voided} ${String(d.player || '').slice(0, 6)}`
    );
  }

  console.log(`\n== hosted history star #${starId} (${rows.length} events) ==`);
  for (const [name, c] of Object.entries(tally).sort()) console.log(`  ${name}: ${c}`);
  console.log(`  resolved ${resolvedN}  ${sol(resolvedAmt)}  novas ${killed}`);
  console.log(`  cancelled ${cancelledN}  ${sol(cancelledAmt)}`);
  console.log('  by player:');
  for (const [pk, v] of [...byPlayer.entries()].filter(([k]) => k).sort((a, b) => b[1].resolved + b[1].cancelled - (a[1].resolved + a[1].cancelled))) {
    console.log(
      `    ${pk}  landed ${v.rn} ${sol(v.resolved)}  refunded ${v.cn} ${sol(v.cancelled)}`
    );
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
