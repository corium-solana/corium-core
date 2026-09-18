/**
 * H-3 (fixed) - a push landing mid-flight no longer moves the round's seed.
 *
 * The bug: `round.entropy` was a running hash, folded again on every arrival. The
 * seed is a function of that field, the ORAO randomness account's address was a
 * function of the seed, and Solana needs every address named before a transaction
 * executes - so the crank had to read the round, derive the address, and hope the
 * field had not moved by the time its draw landed. Any `request_push` confirmed in
 * that gap changed the seed, so the draw failed the address check and reverted.
 *
 * No attacker was required, only traffic, and it got worse the busier the star
 * was: an Open round keeps taking members until somebody draws it, so a hot round
 * kept growing while every attempt kept missing. A round nobody could win the gap
 * on ran to `ROUND_EXPIRY_SLOTS` and refunded its whole batch without ever taking
 * a roll - the game jamming precisely when it was most alive. Anyone who wanted to
 * force that could, for the price of one push per slot, all of it refundable.
 *
 * The fix writes `round.entropy` once, from the `client_seed` of the member who
 * opened the round, and never touches it again. Every other seed input was already
 * either fixed at open or the caller's own choice, so a seed the crank derives now
 * holds still.
 *
 * Moving to MagicBlock retired the address that turned a moved seed into a revert:
 * a request is filed against a queue, not created at an address, and `draw_round`
 * derives the seed on chain rather than being handed one. So the frozen field is
 * no longer what keeps a busy round drawable - it is what keeps the seed
 * *predictable*. The crank derives it off chain to tie a queued request back to
 * the round that made it, and clients replay the same derivation from public data;
 * a field that moved under them would break that tie silently rather than loudly.
 * This scenario derives the seed *before* a second member joins, and shows the
 * round committing exactly that seed afterwards.
 */

import { Keypair } from '@solana/web3.js';

import {
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  drawRound,
  grindSurvival,
  head,
  newPlayer,
  novaPpb,
  pushStatus,
  recentSlotHash,
  requestPush,
  resolvePush,
  roundSeed,
  say,
  showRound,
  sol,
  statusName,
  vrfFulfill,
  waitOutWindow,
  FEED_MASS,
} from '../lib';

const STAKE = 40_000_000; // 0.04 SOL each; two of them clear the draw-cost bar

async function main() {
  head('H-3: a late joiner shares the draw instead of invalidating it');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('alice opens a round, which commits the seed\'s player-supplied half');
  const alice = await newPlayer(w);
  const pAlice = await requestPush(w, 1, STAKE, alice);
  const roundId = pAlice.roundId;
  const opened: any = await w.program.account.round.fetch(w.round(1, roundId));
  const committed = Buffer.from(opened.entropy);
  assert(!committed.every((b) => b === 0), 'round.entropy is set, from her client_seed');
  say(`entropy ${committed.toString('hex').slice(0, 16)}… fixed at open`);

  // Funded up front so the only thing standing between the crank's read and its
  // draw is bob's push itself, which is what the race really was.
  const bob = await newPlayer(w);
  await waitOutWindow(w, 1, roundId);

  act('the crank reads the round and derives the seed it is about to commit');
  // Exactly what `shared/chain/actions.js` does: seed from the round's entropy,
  // a slot hash of its own choosing, and its own key. The program will run the
  // same derivation over the same inputs inside `draw_round`, so this is a
  // prediction that has to survive whatever lands in between.
  const picked = await recentSlotHash(w);
  const predicted = roundSeed(1, roundId, committed, picked.slot, picked.hash, w.payer.publicKey);
  say(`it expects to commit seed ${predicted.toString('hex').slice(0, 16)}…`);

  act('bob joins in that gap - which used to be the whole of the problem');
  const pBob = await requestPush(w, 1, STAKE, bob);
  assert(pBob.roundId === roundId, `bob landed in the same round (#${roundId})`);
  const grown: any = await w.program.account.round.fetch(w.round(1, roundId));
  assert(Number(grown.memberCount) === 2, 'the round grew to two members');
  assert(
    Buffer.from(grown.entropy).equals(committed),
    'and the seed input did not move an inch'
  );

  act('so the draw the crank already built still lands');
  const drew = await drawRound(w, 1, roundId, {
    seedSlot: picked.slot,
    slotHash: picked.hash,
  });
  const sealed = await showRound(w, 1, roundId);
  assert(
    statusName(sealed.status) === 'requested',
    'the round is sealed and paid for, waiting on the callback'
  );
  assert(
    Buffer.from(sealed.seed).equals(predicted),
    'and committed exactly the seed derived before bob existed'
  );

  act('sealing is atomic, so the next push cannot reach the batch it sealed');
  const carol = await newPlayer(w);
  const pCarol = await requestPush(w, 1, STAKE, carol);
  assert(pCarol.roundId === roundId + 1, `carol opened round #${pCarol.roundId} instead`);
  const untouched: any = await w.program.account.round.fetch(w.round(1, roundId));
  assert(Number(untouched.memberCount) === 2, 'the sealed round still has exactly two members');

  act('and both members roll against the one draw they shared');
  const draw = grindSurvival([
    { pushId: pAlice.pushId, thresholdPpb: novaPpb(STAKE, FEED_MASS + STAKE) },
    { pushId: pBob.pushId, thresholdPpb: novaPpb(STAKE, FEED_MASS + 2 * STAKE) },
  ]);
  await vrfFulfill(w, 1, roundId, drew.seed, draw);
  const drawn = await showRound(w, 1, roundId);
  assert(statusName(drawn.status) === 'drawn', 'the callback landed and the round is Drawn');
  await resolvePush(w, pAlice);
  await resolvePush(w, pBob);
  assert((await pushStatus(w, pAlice)) === 'survived', 'alice settled against the draw');
  assert((await pushStatus(w, pBob)) === 'survived', 'bob settled against the same draw');

  act('what the fix is worth');
  say(`bob staked ${sol(STAKE)} and cost the round nothing but a member slot`);
  say('against the address-derived oracle his push reverted the crank\'s draw, and');
  say('  the next attempt raced the next arrival - on a busy star, until the batch');
  say('  expired into refunds');
  say('a filed request cannot revert that way, so what the frozen field buys now is');
  say('  a seed anyone can derive for themselves and find on chain unmoved');
  say('nothing about the seed got weaker: alice picked her half blind, and the');
  say('  slot hash it is mixed with did not exist when she signed');

  process.exit(conclude('H-3'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
