/**
 * Regression for the C-1 fix.
 *
 * Making the refund branch respect `settle_cursor` is a real behaviour change:
 * refunds used to be accepted in any order, and the biggest population of them
 * is the queue sitting behind a supernova. Every one of those pushes takes the
 * refund path because the star is finished, and if ordering broke that drain the
 * fix would be worse than the bug.
 *
 * So: force a kill at the head of a three-member round with a ground draw, then
 * drain the two behind it and make sure the money all lands - including the
 * winner's prize.
 */

import { Keypair, SystemProgram } from '@solana/web3.js';

import {
  act,
  assert,
  bootstrap,
  closeRoundAccount,
  conclude,
  connect,
  drawRound,
  grindRoll,
  head,
  mustFail,
  newPlayer,
  novaPpb,
  pushStatus,
  requestPush,
  resolvePush,
  rollFor,
  say,
  showStar,
  sol,
  statusName,
  vrfFulfill,
  waitOutWindow,
  FEED_MASS,
} from '../lib';

const STAKE = 40_000_000; // 0.04 SOL each; three of them clear the draw bar

async function main() {
  head('REGRESSION: a supernova mid-queue still drains everyone behind it');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('three players join one round');
  const alice = await newPlayer(w);
  const bob = await newPlayer(w);
  const carol = await newPlayer(w);
  const pAlice = await requestPush(w, 1, STAKE, alice);
  const pBob = await requestPush(w, 1, STAKE, bob);
  const pCarol = await requestPush(w, 1, STAKE, carol);
  const roundId = pAlice.roundId;
  const bobStart = await w.connection.getBalance(bob.publicKey);
  const carolStart = await w.connection.getBalance(carol.publicKey);
  say(`push ids ${pAlice.pushId}, ${pBob.pushId}, ${pCarol.pushId} in round #${roundId}`);

  act('seal, buy the draw, and grind a draw that kills the star on the first push');
  await waitOutWindow(w, 1, roundId);
  const { seed } = await drawRound(w, 1, roundId);

  const threshold = novaPpb(STAKE, FEED_MASS + STAKE);
  const draw = grindRoll(pAlice.pushId, threshold);
  say(`alice's threshold is ${threshold} ppb (${(threshold / 1e7).toFixed(2)}%)`);
  say(`ground a draw where her roll is ${rollFor(draw, pAlice.pushId)} ppb - lethal`);
  await vrfFulfill(w, 1, roundId, seed, draw);

  act('alice settles and takes the star with her');
  await resolvePush(w, pAlice);
  const dead = await showStar(w, 1, 'after the kill');
  assert(statusName(dead.status) === 'dead', `the star novaed (status=${statusName(dead.status)})`);
  assert((await pushStatus(w, pAlice)) === 'killed', 'alice holds the lethal push');
  say(`final prize ${sol(dead.finalPrize)}`);

  act('the two behind her refund - in queue order, on a finished star');
  const outOfTurn = await mustFail('refund carol before bob', () => resolvePush(w, pCarol));
  assert(outOfTurn === 'PushOutOfOrder', 'ordering is enforced on the post-death drain too');

  await resolvePush(w, pBob);
  await resolvePush(w, pCarol);
  const bobBack = (await w.connection.getBalance(bob.publicKey)) - bobStart;
  const carolBack = (await w.connection.getBalance(carol.publicKey)) - carolStart;
  assert((await pushStatus(w, pBob)) === 'cancelled', `bob refunded (${sol(bobBack)})`);
  assert((await pushStatus(w, pCarol)) === 'cancelled', `carol refunded (${sol(carolBack)})`);
  assert(bobBack >= STAKE - 10_000 && carolBack >= STAKE - 10_000, 'both got their full stake');

  act('and the winner can collect');
  const before = await w.connection.getBalance(alice.publicKey);
  await w.program.methods
    .claimPrize()
    .accountsPartial({
      winner: alice.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(1),
      playerStats: w.playerStats(alice.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .signers([alice])
    .rpc();
  const prize = (await w.connection.getBalance(alice.publicKey)) - before;
  say(`alice claimed ${sol(prize)}`);
  assert(prize > 0, 'the prize paid out');

  act('and the round\'s rent goes home now that it has drained');
  const roundRent = (await w.connection.getAccountInfo(w.round(1, roundId)))!.lamports;
  const openerBefore = await w.connection.getBalance(alice.publicKey);
  await closeRoundAccount(w, 1, roundId);
  const returned = (await w.connection.getBalance(alice.publicKey)) - openerBefore;
  assert(returned === roundRent, `alice opened the round and got its ${sol(roundRent)} back`);

  act('the books balance');
  const cfg: any = await w.program.account.config.fetch(w.config);
  const vault = await w.connection.getBalance(w.vault);
  const liabilities =
    Number(cfg.pendingLiability) +
    Number(cfg.prizeLiability) +
    Number(cfg.protocolAccrued) +
    Number(cfg.nextStarReserve);
  say(
    `vault ${sol(vault)} vs liabilities ${sol(liabilities)} ` +
      `(pending ${sol(cfg.pendingLiability)}, prize ${sol(cfg.prizeLiability)}, ` +
      `protocol ${sol(cfg.protocolAccrued)}, reserve ${sol(cfg.nextStarReserve)})`
  );
  assert(Number(cfg.pendingLiability) === 0, 'no escrow left over');
  assert(vault >= liabilities, 'the vault still covers every liability');

  process.exit(conclude('REGRESSION'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
