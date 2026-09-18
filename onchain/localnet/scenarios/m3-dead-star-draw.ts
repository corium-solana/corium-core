/**
 * M-3 (fixed) - no draw is bought for a star that is already finished.
 *
 * The bug: `close_round` checked the round and the draw economics but never the
 * star's status. A star that died with an open round left a batch that still
 * passed `closeable_at` and, if it had raked enough, got sealed and drawn -
 * spending the float on randomness no path would ever read, because the refund
 * branch in `resolve_push` fires on `star.is_finished()` before it looks at the
 * round at all. It also made those members wait for a draw that changed nothing.
 *
 * The fix is one `require!(star.is_alive())` in `draw_round`, which is strictly
 * better for players: they refund immediately instead of waiting. This scenario
 * kills a star with a fully-funded round still open behind it and shows the draw
 * being refused while the refunds go through anyway.
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
  showRound,
  showStar,
  sol,
  statusName,
  vrfFulfill,
  waitOutWindow,
  FEED_MASS,
} from '../lib';

const STAKE = 80_000_000; // 0.08 SOL - one push clears the draw-cost bar on its own

async function main() {
  head('M-3: a finished star will not buy randomness nobody reads');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('round A: one player, a ground draw, and the star dies');
  const alice = await newPlayer(w);
  const pAlice = await requestPush(w, 1, STAKE, alice);
  const roundA = pAlice.roundId;
  await waitOutWindow(w, 1, roundA);
  const { seed } = await drawRound(w, 1, roundA);
  const threshold = novaPpb(STAKE, FEED_MASS + STAKE);
  const draw = grindRoll(pAlice.pushId, threshold);
  say(`alice's roll will be ${rollFor(draw, pAlice.pushId)} ppb against ${threshold} ppb - lethal`);
  await vrfFulfill(w, 1, roundA, seed, draw);

  act('round B opens behind her, with more than enough stake to pay for a draw');
  const bob = await newPlayer(w);
  const pBob = await requestPush(w, 1, STAKE, bob);
  const roundB = pBob.roundId;
  assert(roundB !== roundA, `bob is in a later round (#${roundB}) than alice (#${roundA})`);

  act('alice settles and takes the star with her');
  await resolvePush(w, pAlice);
  const dead = await showStar(w, 1, 'after the kill');
  assert(statusName(dead.status) === 'dead', `the star novaed (status=${statusName(dead.status)})`);

  act('round B is drawable by every other measure, and is refused anyway');
  await waitOutWindow(w, 1, roundB);
  const open = await showRound(w, 1, roundB);
  assert(statusName(open.status) === 'open', 'round B is still Open and past its window');
  const cfgBefore: any = await w.program.account.config.fetch(w.config);
  const code = await mustFail('draw_round on a finished star', () => drawRound(w, 1, roundB));
  assert(code === 'StarNotAlive', 'refused with StarNotAlive');

  act('nothing was spent, and bob did not have to wait for a draw to get out');
  const beforeRefund = await w.connection.getBalance(bob.publicKey);
  await resolvePush(w, pBob);
  const cfgAfter: any = await w.program.account.config.fetch(w.config);
  const bobBack = (await w.connection.getBalance(bob.publicKey)) - beforeRefund;
  assert((await pushStatus(w, pBob)) === 'cancelled', `bob refunded (${sol(bobBack)})`);
  assert(bobBack === STAKE, 'his whole stake, to the lamport, with no randomness involved');
  assert(
    Number(cfgAfter.protocolAccrued) >= Number(cfgBefore.protocolAccrued),
    'the protocol float did not pay for a draw nobody read'
  );
  const stillOpen: any = await w.program.account.round.fetch(w.round(1, roundB));
  assert(
    Buffer.from(stillOpen.seed).every((b) => b === 0),
    'round B never committed a seed, so no request was ever filed for it'
  );

  act('and its rent still comes back, even though it never drew');
  const rent = (await w.connection.getAccountInfo(w.round(1, roundB)))!.lamports;
  const before = await w.connection.getBalance(bob.publicKey);
  await closeRoundAccount(w, 1, roundB);
  assert(
    (await w.connection.getBalance(bob.publicKey)) - before === rent,
    `bob opened round B and got its ${sol(rent)} back`
  );

  act('the winner still collects, so nothing about the kill path changed');
  const prizeBefore = await w.connection.getBalance(alice.publicKey);
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
  const prize = (await w.connection.getBalance(alice.publicKey)) - prizeBefore;
  assert(prize > 0, `alice claimed ${sol(prize)}`);

  const cfg: any = await w.program.account.config.fetch(w.config);
  assert(Number(cfg.pendingLiability) === 0, 'no escrow left over');

  process.exit(conclude('M-3'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
