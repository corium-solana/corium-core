/**
 * List push accounts and where their round has got to.
 *
 * A push does not own its randomness any more - its round does, and one draw
 * settles every member of that round. So the interesting state is the round's.
 *
 *   yarn show-pending                 # only unresolved
 *   yarn show-pending --all           # every push the program has ever made
 *   yarn show-pending --star 2
 *   yarn show-pending --player <pubkey>
 */

import { drawBreakEvenStake } from '../../shared/chain/economics.js';
import { fetchDrawCost } from '../../shared/chain/orao.js';

import {
  context,
  enumName,
  fetchRandomness,
  parseArgs,
  roundPda,
  sol,
  ts,
} from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, connection } = context(args);
  const drawCost = await fetchDrawCost(connection);

  const roundCache = new Map<string, any>();
  const loadRound = async (starId: any, roundId: any) => {
    const key = `${starId}:${roundId}`;
    if (!roundCache.has(key)) {
      const pk = roundPda(programId, starId.toString(), roundId.toString());
      const account = await program.account.round.fetch(pk).catch(() => null);
      roundCache.set(key, account ? { pk, account } : null);
    }
    return roundCache.get(key);
  };

  let pushes = await program.account.pendingPush.all();

  if (!args.all) {
    pushes = pushes.filter((p) => enumName(p.account.status) === 'pending');
  }
  if (args.star) {
    pushes = pushes.filter((p) => p.account.starId.toString() === String(args.star));
  }
  if (args.player) {
    pushes = pushes.filter((p) => p.account.player.toBase58() === String(args.player));
  }

  pushes.sort((a, b) => {
    const s = Number(a.account.starId) - Number(b.account.starId);
    return s !== 0 ? s : Number(a.account.pushId) - Number(b.account.pushId);
  });

  if (pushes.length === 0) {
    console.log(args.all ? 'No pushes found.' : 'No pending pushes.');
    return;
  }

  for (const p of pushes) {
    const a = p.account;
    const status = enumName(a.status);
    console.log(`push ${p.publicKey.toBase58()}`);
    console.log(`  star #${a.starId}  push #${a.pushId}  round #${a.roundId}  ${status.toUpperCase()}`);
    console.log(`  player     ${a.player.toBase58()}`);
    console.log(`  amount     ${sol(a.amount)}`);
    console.log(`  requested  ${ts(a.requestedTs)} (slot ${a.requestedSlot})`);

    if (status === 'pending') {
      const round = await loadRound(a.starId, a.roundId);
      if (!round) {
        console.log('  round      MISSING (account not found)');
      } else {
        const rs = enumName(round.account.status);
        console.log(
          `  round      ${round.pk.toBase58()} ${rs.toUpperCase()} (${round.account.memberCount} members share one draw)`
        );
        if (rs === 'open') {
          // Two different reasons an open round has not sealed, and the
          // difference is the whole answer to "why is my push still pending".
          const stake = BigInt(round.account.stake.toString());
          const needed = BigInt(drawBreakEvenStake(drawCost));
          console.log(
            stake >= needed
              ? '  vrf        not sealed yet - still taking entrants'
              : `  vrf        not sealed yet - batch at ${sol(stake)} of ${sol(needed)} needed to pay for a draw`
          );
        } else if (rs === 'expired') {
          console.log('  vrf        round VOIDED - refundable now, no randomness needed');
        } else {
          const r = await fetchRandomness(connection, round.account.randomness);
          const state = !r.exists
            ? 'MISSING (request account not found)'
            : r.fulfilled
              ? 'FULFILLED - ready to resolve'
              : 'waiting for ORAO oracles';
          console.log(`  randomness ${round.account.randomness.toBase58()}`);
          console.log(`  vrf        ${state}`);
        }
      }
    } else {
      console.log(`  resolved   ${ts(a.resolvedTs)} (slot ${a.resolvedSlot})`);
      if (status !== 'cancelled') {
        console.log(`  roll       ${a.rollPpb} ppb vs threshold ${a.thresholdPpb} ppb`);
      }
    }
    console.log();
  }

  console.log(`${pushes.length} push account(s).`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
