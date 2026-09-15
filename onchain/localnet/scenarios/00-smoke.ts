/**
 * Baseline: does a normal last-hit work end to end against the mock oracle?
 *
 * Nothing adversarial here. If this does not pass, every finding below it is
 * measuring a broken harness rather than a broken program.
 */

import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';

import {
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  head,
  newPlayer,
  pushStatus,
  requestPush,
  resolvePush,
  say,
  sealAndDraw,
  showRound,
  showStar,
  sol,
  stakeNeededForDraw,
} from '../lib';

async function main() {
  head('SMOKE: one push, one round, one draw, one settle');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  const need = await stakeNeededForDraw(w);
  say(`a round must stake ${sol(need)} before its rake covers one ORAO draw`);

  act('one player pushes enough to pay for a draw');
  const alice = await newPlayer(w);
  const before = await w.connection.getBalance(alice.publicKey);
  const p = await requestPush(w, 1, need, alice);
  say(`push_id=${p.pushId} round=${p.roundId} amount=${sol(p.amount)}`);
  await showStar(w, 1, 'after push');

  act('seal the round, buy the draw, let ORAO answer');
  await sealAndDraw(w, 1, p.roundId, treasury);
  await showRound(w, 1, p.roundId);

  act('settle');
  await resolvePush(w, p);
  const status = await pushStatus(w, p);
  const star = await showStar(w, 1, 'after settle');
  const after = await w.connection.getBalance(alice.publicKey);
  say(`alice net: ${sol(after - before)} (stake ${sol(p.amount)} plus rent and fees)`);

  assert(status !== 'pending', `push resolved (status=${status})`);
  assert(
    Number(star.settleCursor) === p.pushId + 1,
    `settle_cursor advanced past the push (${star.settleCursor})`
  );
  assert(Number(star.totalMass) > LAMPORTS_PER_SOL, `mass grew to ${sol(star.totalMass)}`);

  process.exit(conclude('SMOKE'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
