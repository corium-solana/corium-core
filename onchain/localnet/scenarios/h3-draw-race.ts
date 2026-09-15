/**
 * H-3 (fixed) - a push landing mid-flight no longer reverts the draw.
 *
 * The bug: `round.entropy` was a running hash, folded again on every arrival. The
 * ORAO account address is a function of the seed, the seed is a function of that
 * field, and Solana needs every address named before a transaction executes - so
 * the crank had to read the round, derive the address, and hope the field had not
 * moved by the time its draw landed. Any `request_push` confirmed in that gap
 * changed the seed, so the draw failed the address check and reverted.
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
 * either fixed at open or the caller's own choice, so an address the crank derives
 * now holds still. This scenario derives one *before* a second member joins, and
 * shows the draw landing on it afterwards.
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
  oraoFulfill,
  pushStatus,
  recentSlotHash,
  requestPush,
  resolvePush,
  roundSeed,
  say,
  showRound,
  sol,
  statusName,
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

  act('the crank reads the round and derives the address it is about to request');
  // Exactly what `shared/chain/actions.js` does: seed from the round's entropy,
  // a slot hash of its own choosing, and its own key.
  const picked = await recentSlotHash(w);
  const predicted = roundSeed(1, roundId, committed, picked.slot, picked.hash, w.payer.publicKey);
  const predictedAddress = w.oraoRequest(predicted);
  say(`it will ask ORAO for ${predictedAddress.toBase58()}`);

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
  const drew = await drawRound(w, 1, roundId, treasury, {
    seedSlot: picked.slot,
    slotHash: picked.hash,
  });
  assert(
    drew.request.equals(predictedAddress),
    'the draw went to the address derived before bob existed'
  );
  const sealed = await showRound(w, 1, roundId);
  assert(statusName(sealed.status) === 'requested', 'the round is sealed and its draw bought');
  assert(
    Buffer.from(sealed.seed).equals(predicted),
    'and committed exactly the seed that address came from'
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
  await oraoFulfill(w, drew.request, draw);
  await resolvePush(w, pAlice);
  await resolvePush(w, pBob);
  assert((await pushStatus(w, pAlice)) === 'survived', 'alice settled against the draw');
  assert((await pushStatus(w, pBob)) === 'survived', 'bob settled against the same draw');

  act('what the fix is worth');
  say(`bob staked ${sol(STAKE)} and cost the round nothing but a member slot`);
  say('before it, his push reverted the crank\'s draw, and the next attempt raced');
  say('  the next arrival - on a busy star, until the batch expired into refunds');
  say('nothing about the seed got weaker: alice picked her half blind, and the');
  say('  slot hash it is mixed with did not exist when she signed');

  process.exit(conclude('H-3'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
