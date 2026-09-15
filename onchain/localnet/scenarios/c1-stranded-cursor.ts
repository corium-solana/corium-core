/**
 * C-1 (fixed) - a refund may not jump the settle queue.
 *
 * The bug: `resolve_push` took the refund branch *before* checking
 * `push_id == star.settle_cursor`, but still advanced the cursor. So a push that
 * refunded out of order moved the cursor past a lower `push_id` that had not
 * settled yet. Every writer of `settle_cursor` only ever increments it, so the
 * skipped push could never satisfy the ordering check again, and its only other
 * exit - the refund branch - needs either a finished star or an expired round.
 * A round whose draw already landed cannot expire. There was no exit at all, and
 * because the star's mass then froze it could never finish, so no successor star
 * could be born either: one impatient player halted the whole game.
 *
 * Nothing adversarial was required. The sequence below is exactly what a crank
 * outage plus one impatient player produces. The fix hoists the ordering check
 * above both branches, so this now runs to a clean drain.
 */

import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';

import {
  ROUND_EXPIRY_SLOTS,
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  expireRound,
  head,
  mustFail,
  newPlayer,
  pushStatus,
  requestPush,
  resolvePush,
  say,
  sealAndDraw,
  showRound,
  showStar,
  waitUntilSlot,
} from '../lib';

const STAKE = LAMPORTS_PER_SOL / 25; // 0.04 SOL, two of them clear the draw bar

async function main() {
  head('C-1: an out-of-order refund stranding an earlier round forever');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('round A: two players queue up and their draw lands normally');
  const alice = await newPlayer(w);
  const bob = await newPlayer(w);
  const aliceBefore = await w.connection.getBalance(alice.publicKey);
  const pAlice = await requestPush(w, 1, STAKE, alice);
  const pBob = await requestPush(w, 1, STAKE, bob);
  say(`alice push_id=${pAlice.pushId}, bob push_id=${pBob.pushId}, round #${pAlice.roundId}`);
  const roundA = pAlice.roundId;
  await sealAndDraw(w, 1, roundA, treasury);
  say('round A is Requested with a landed draw - it can never be expired again');
  say('the crank now dies before resolving anybody');

  act('round B: two more players queue up, and nobody buys their draw');
  const carol = await newPlayer(w);
  const dave = await newPlayer(w);
  const pCarol = await requestPush(w, 1, STAKE, carol);
  const pDave = await requestPush(w, 1, STAKE, dave);
  const roundB = pCarol.roundId;
  say(`carol push_id=${pCarol.pushId}, dave push_id=${pDave.pushId}, round #${roundB}`);
  await showStar(w, 1, 'both rounds queued');

  act(`carol waits out the expiry and takes her money back - the documented escape hatch`);
  const rb: any = await w.program.account.round.fetch(w.round(1, roundB));
  await waitUntilSlot(
    w,
    Number(rb.openedSlot) + ROUND_EXPIRY_SLOTS,
    `round B expiry (${ROUND_EXPIRY_SLOTS} slots)`
  );
  await expireRound(w, 1, roundB);
  await showRound(w, 1, roundB);

  const refused = await mustFail(
    `refund carol out of turn (push_id ${pCarol.pushId}, cursor ${pAlice.pushId})`,
    () => resolvePush(w, pCarol)
  );
  assert(refused === 'PushOutOfOrder', 'the refund branch now respects the queue');

  act('so the queue drains from the head instead, and everyone gets out');
  for (const [who, p] of [
    ['alice', pAlice],
    ['bob', pBob],
    ['carol', pCarol],
    ['dave', pDave],
  ] as const) {
    await resolvePush(w, p);
    say(`${who} (push_id ${p.pushId}) resolved as ${await pushStatus(w, p)}`);
  }
  const drained = await showStar(w, 1, 'after the drain');
  const cfgFixed: any = await w.program.account.config.fetch(w.config);
  assert(
    (await pushStatus(w, pAlice)) !== 'pending',
    'alice reached her landed draw instead of being stranded'
  );
  assert(Number(drained.pendingPushes) === 0, 'no push is left pending');
  assert(Number(cfgFixed.pendingLiability) === 0, 'no escrow is left in the vault');
  process.exit(conclude('C-1'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
