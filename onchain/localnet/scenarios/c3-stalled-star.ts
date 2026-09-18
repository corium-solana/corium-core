/**
 * C-3 - nobody's SOL gets stuck when the oracle dies, or when the game does.
 *
 * `ROUND_EXPIRY_SLOTS` gets every *stake* out of a round the oracle never
 * answered, and that used to be the whole liveness story. It is not enough: a
 * refund lands no mass, so a star whose oracle never comes back hands every
 * stake back and then sits Alive forever, holding the pot that earlier pushes
 * already paid in. No kill to pay a winner, no Event Horizon to pay the feeders,
 * and no successor either, because a new star needs the incumbent closed. The
 * feeders' SOL is stuck, and with upgrade authority gone there is nothing to fix
 * it with. Exactly the same shape appears with nothing broken at all, if people
 * simply stop playing.
 *
 * `collapse_stalled_star` is the floor under both. After a week without new mass
 * - a day for a star still in its nursery - anyone may finish the star. Feeders
 * claim back the prize-side value of their own feeds, and the pot the settled
 * pushes built recycles into the next star's endowment.
 *
 * Run against a `short-stalls` build, where those windows are 25 and 10 seconds:
 *
 *     localnet/build-short-stalls.sh
 *     SOLDUST_SO=localnet/soldust-short-stalls.so localnet/run.sh \
 *       localnet/scenarios/c3-stalled-star.ts
 *
 * `run-all.sh` does that for you. The shipped day-scale values are pinned by a
 * unit test that only compiles when the feature is off, so this build cannot be
 * the one that deploys.
 */

import { BN } from '@coral-xyz/anchor';
import { Keypair, LAMPORTS_PER_SOL, SystemProgram } from '@solana/web3.js';

import {
  act,
  assert,
  conclude,
  connect,
  createFirstStar,
  drawRound,
  expireRound,
  feed,
  fundProtocol,
  grindSurvival,
  head,
  initialize,
  mustFail,
  newPlayer,
  novaPpb,
  pushStatus,
  requestPush,
  resolvePush,
  say,
  showStar,
  sol,
  stakeNeededForDraw,
  statusName,
  vrfFulfill,
  vrfInit,
  waitOutWindow,
  waitUntilSlot,
  FEED_MASS,
  NURSERY_STALL_SECS,
  PROTOCOL_BPS,
  PUSH_STEP,
  ROUND_EXPIRY_SLOTS,
  STALL_SECS,
} from '../lib';

/** The prize-side value of an amount: what `Economics::split` leaves behind. */
const prizeOf = (lamports: number) => lamports - Math.floor((lamports * PROTOCOL_BPS) / 10_000);

const ALICE_FEED = 600_000_000; // 0.6 SOL
const BOB_FEED = FEED_MASS - ALICE_FEED; // 0.4, filling the nursery exactly

/**
 * Cluster time, as the program sees it. The stall clock is `Clock::unix_timestamp`
 * rather than a slot count, because these windows are day-scale on mainnet and
 * slot time drifts too much to count a week in slots.
 */
async function chainTime(w: any) {
  let s = await w.connection.getSlot('confirmed');
  // A skipped slot has no block and therefore no time; walk back rather than
  // returning a zero that would turn the wait below into a hang.
  for (let i = 0; i < 8; i++, s--) {
    const t = await w.connection.getBlockTime(s);
    if (t) return t;
  }
  throw new Error('no block time in the last 8 slots');
}

/** Wait until the star has been silent for its whole stall window. */
async function waitOutStall(w: any, starId: number, window: number, label: string) {
  const star: any = await w.program.account.star.fetch(w.star(starId));
  const due = Number(star.lastMassTs) + window;
  let now = await chainTime(w);
  if (now >= due) return now;
  say(`waiting ${due - now}s for the ${label} (last mass at ts ${star.lastMassTs})`);
  const deadline = Date.now() + (window + 60) * 1_000;
  while (now < due) {
    if (Date.now() > deadline) {
      throw new Error(
        `cluster time is not advancing (${now} vs ${due}); is this a short-stalls build?`
      );
    }
    await new Promise((r) => setTimeout(r, 1_000));
    now = await chainTime(w);
  }
  return now;
}

function collapse(w: any, starId: number) {
  return w.program.methods
    .collapseStalledStar()
    .accountsPartial({
      cranker: w.payer.publicKey,
      config: w.config,
      star: w.star(starId),
      starFeed: w.starFeed(starId),
    })
    .rpc();
}

function claimHole(w: any, starId: number, player: Keypair) {
  return w.program.methods
    .claimHoleShare()
    .accountsPartial({
      player: player.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(starId),
      starFeed: w.starFeed(starId),
      feedShare: w.feedShare(starId, player.publicKey),
      playerStats: w.playerStats(player.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .signers([player])
    .rpc();
}

async function main() {
  head('C-3: a star that stops moving can always be finished, by anyone');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;

  act('two feeders fill the nursery of star #1');
  await vrfInit(w);
  await initialize(w, treasury);
  await fundProtocol(w, 2 * LAMPORTS_PER_SOL);
  await createFirstStar(w);
  const alice = await newPlayer(w);
  const bob = await newPlayer(w);
  await feed(w, 1, ALICE_FEED, alice);
  await feed(w, 1, BOB_FEED, bob);
  say(`alice fed ${sol(ALICE_FEED)}, bob ${sol(BOB_FEED)} - the hole is theirs 60/40`);

  act('a live star is not collapsible, however quiet the caller claims it is');
  assert(
    (await mustFail('collapse a star that just gained mass', () => collapse(w, 1))) ===
      'StarNotStalled',
    'the fuse is real: mass landed this second'
  );

  act('one push lands, so the pot stops being only the feeders\' own money');
  const stake = await stakeNeededForDraw(w);
  const pAlice = await requestPush(w, 1, stake, alice);
  await waitOutWindow(w, 1, pAlice.roundId);
  const drew = await drawRound(w, 1, pAlice.roundId);
  await vrfFulfill(
    w,
    1,
    pAlice.roundId,
    drew.seed,
    grindSurvival([{ pushId: pAlice.pushId, thresholdPpb: novaPpb(stake, FEED_MASS + stake) }])
  );
  await resolvePush(w, pAlice);
  assert((await pushStatus(w, pAlice)) === 'survived', `alice pushed ${sol(stake)} and survived`);
  const settled = await showStar(w, 1, 'star #1');
  assert(
    Number(settled.totalMass) === FEED_MASS + stake,
    'her stake is mass now, and its prize share belongs to whoever kills the star'
  );

  act('then the oracle goes dark: a draw is bought and the callback never comes');
  const pBob = await requestPush(w, 1, stake, bob);
  const bobPaid = await w.connection.getBalance(bob.publicKey);
  await waitOutWindow(w, 1, pBob.roundId);
  const orphan = await drawRound(w, 1, pBob.roundId);
  const waiting: any = await w.program.account.round.fetch(w.round(1, pBob.roundId));
  assert(
    statusName(waiting.status) === 'requested',
    `round #${pBob.roundId} is Requested - paid for, on the queue, and there it stays`
  );
  say(`its seed is ${orphan.seed.toString('hex').slice(0, 16)}..., and nothing ever answers it`);

  act('the star goes quiet, but a queued push still blocks the collapse');
  await waitOutStall(w, 1, STALL_SECS, 'stall window');
  assert(
    (await mustFail('collapse over the top of a pending push', () => collapse(w, 1))) ===
      'StarQueueNotEmpty',
    'the timer has run and it still refuses: bob has a roll coming'
  );

  act('bob gets his stake back the ordinary way, on slots alone');
  const round: any = await w.program.account.round.fetch(w.round(1, pBob.roundId));
  await waitUntilSlot(
    w,
    Number(round.requestedSlot) + ROUND_EXPIRY_SLOTS,
    `round #${pBob.roundId} expiry (${ROUND_EXPIRY_SLOTS} slots)`
  );
  await expireRound(w, 1, pBob.roundId);
  await resolvePush(w, pBob);
  assert((await pushStatus(w, pBob)) === 'cancelled', 'his push refunded without any randomness');
  const bobBack = (await w.connection.getBalance(bob.publicKey)) - bobPaid;
  assert(bobBack >= stake - 10_000, `he is whole again (+${sol(bobBack)})`);

  act('and now the star is finishable - this is the state that used to be forever');
  const stalled = await showStar(w, 1, 'star #1');
  assert(statusName(stalled.status) === 'alive', 'still alive, below the hole cap');
  assert(Number(stalled.pendingPushes) === 0, 'queue drained');
  const pot = Number(stalled.prizePool);
  const claimable = prizeOf(FEED_MASS); // the feeders fed exactly FEED_MASS
  const recycled = pot - claimable;
  const cfgBefore: any = await w.program.account.config.fetch(w.config);
  const vaultBefore = await w.connection.getBalance(w.vault);

  await collapse(w, 1);

  const dead = await showStar(w, 1, 'star #1');
  assert(statusName(dead.status) === 'stalled', 'collapsed as stalled');
  assert(
    Number(dead.finalPrize) === claimable,
    `feeders are owed ${sol(claimable)} - the prize-side value of exactly what they fed`
  );
  assert(
    Buffer.from(dead.deathRandomness).every((b: number) => b === 0),
    'nothing was rolled, and the account says so'
  );

  act('the pot the pushes built is recycled, not paid to anyone');
  const cfgAfter: any = await w.program.account.config.fetch(w.config);
  assert(
    Number(cfgAfter.nextStarReserve) - Number(cfgBefore.nextStarReserve) === recycled,
    `${sol(recycled)} moved into the next star's endowment`
  );
  assert(
    Number(cfgBefore.prizeLiability) - Number(cfgAfter.prizeLiability) === recycled,
    'out of prize_liability by the same amount, so the books balance'
  );
  assert(
    recycled === prizeOf(stake),
    'and it is exactly the prize share of the one push that settled - to the lamport'
  );
  assert(
    (await w.connection.getBalance(w.vault)) === vaultBefore,
    'not one lamport left the vault: a collapse only reclassifies what is owed'
  );
  assert(
    Number(cfgAfter.protocolAccrued) === Number(cfgBefore.protocolAccrued),
    'the house took nothing on the way past'
  );

  act('the feeders take their money back, at cost, and no more');
  for (const [who, name, fed] of [
    [alice, 'alice', ALICE_FEED],
    [bob, 'bob', BOB_FEED],
  ] as [Keypair, string, number][]) {
    const before = await w.connection.getBalance(who.publicKey);
    await claimHole(w, 1, who);
    const got = (await w.connection.getBalance(who.publicKey)) - before;
    const want = Math.floor((fed * claimable) / FEED_MASS);
    assert(got >= want - 10_000, `${name} claimed ${sol(got)} on a ${sol(fed)} feed`);
    assert(want === prizeOf(fed), `  which is ${sol(fed)} less the same 314 bps every push pays`);
  }
  const drained: any = await w.program.account.config.fetch(w.config);
  say(`prize_liability left: ${sol(drained.prizeLiability)}`);

  act('a stalled star is done: no pushes, no feeds, no second collapse');
  assert(
    (await mustFail('push a stalled star', () => requestPush(w, 1, PUSH_STEP, alice))) ===
      'StarNotAlive',
    'it cannot take another stake'
  );
  assert(
    (await mustFail('feed a stalled star', () => feed(w, 1, PUSH_STEP, alice))) === 'StarNotAlive',
    'and it cannot take another feed, so early_volume is frozen'
  );
  assert(
    (await mustFail('collapse it twice', () => collapse(w, 1))) === 'StarNotAlive',
    'and it cannot be collapsed again'
  );

  act('play moves on, funded by the residue');
  await w.program.methods
    .createNextStar(new BN(2))
    .accountsPartial({
      payer: w.payer.publicKey,
      config: w.config,
      prevStar: w.star(1),
      nextStar: w.star(2),
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  const born = await showStar(w, 2, 'star #2');
  const endowment = Math.floor(recycled / PUSH_STEP) * PUSH_STEP;
  assert(
    Number(born.totalMass) === endowment,
    `star #2 was born with ${sol(endowment)} of the residue as mass`
  );

  act('and a star that never leaves its nursery gets the day fuse, not the week');
  const carol = await newPlayer(w);
  await feed(w, 2, PUSH_STEP * 5, carol);
  assert(
    (await mustFail('collapse a freshly fed star', () => collapse(w, 2))) === 'StarNotStalled',
    'its clock restarted on carol\'s feed'
  );
  await waitOutStall(w, 2, NURSERY_STALL_SECS, 'nursery stall window');
  const carolBefore = await w.connection.getBalance(carol.publicKey);
  await collapse(w, 2);
  await claimHole(w, 2, carol);
  const carolBack = (await w.connection.getBalance(carol.publicKey)) - carolBefore;
  assert(carolBack >= prizeOf(PUSH_STEP * 5) - 10_000, `carol got ${sol(carolBack)} of 0.05 back`);
  const end: any = await w.program.account.config.fetch(w.config);
  assert(
    Number(end.nextStarReserve) >= endowment,
    `the endowment rolled on again (${sol(end.nextStarReserve)} waiting for star #3)`
  );

  act('what this buys');
  say('the oracle can die permanently and every lamport still has a route out:');
  say('  stakes refund on slots, the pot refunds feeders at cost, the rest rolls on');
  say('nobody profits from a stall - a collapse pays a feeder 96.86% of their own');
  say('  feed and cancels the 21:1 hole ticket they were holding, which is strictly');
  say('  worse for them than the star continuing. So the long fuse traps no one.');
  say('and the house cannot profit either: the residue is prize money, which is');
  say('  not withdrawable, so it can only ever fund the next star');

  process.exit(conclude('C-3'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
