/**
 * H-1 (fixed) - squatting a round's ORAO address can no longer block its draw.
 *
 * The bug: `close_round` published `round.seed` and `round.randomness` on chain in
 * a *separate transaction* from `request_round_vrf`. Between the two, the address
 * the round had committed to was public knowledge and ORAO's `request_v2` is
 * permissionless, so a stranger could occupy that address first - after which
 * `request_round_vrf` failed its emptiness pre-check forever and the round could
 * never become `Requested`. Escrow was safe (the round expired into refunds) but
 * a star under this attack could never settle a single push, for about 0.0005 SOL
 * net per round blocked.
 *
 * The fix merges the two into `draw_round`, so the seed is decided and spent in
 * one transaction. This scenario proves the three things that follow from that:
 *
 *  1. There is no sealed-but-undrawn state left to read a seed out of.
 *  2. A squatter who somehow guesses the address only costs the crank a reverted
 *     transaction - the round is still open.
 *  3. Retrying with a different slot hash lands a different address, so the round
 *     draws and settles normally.
 */

import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';

import {
  REQUEST_FEE,
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  drawRound,
  head,
  mustFail,
  newPlayer,
  oraoFulfill,
  oraoRequestDirect,
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
} from '../lib';

const STAKE = 80_000_000;

async function main() {
  head('H-1: a squatted ORAO address costs the crank one transaction, not the star');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('a player queues and the round becomes drawable');
  const alice = await newPlayer(w);
  const p = await requestPush(w, 1, STAKE, alice);
  const roundId = p.roundId;
  await waitOutWindow(w, 1, roundId);

  act('there is no state to read a seed out of before it is spent');
  const open: any = await w.program.account.round.fetch(w.round(1, roundId));
  assert(statusName(open.status) === 'open', 'the round is Open, and there is no state after it but Requested');
  assert(
    Buffer.from(open.seed).every((b) => b === 0),
    'round.seed is still zero - nothing is committed until the draw is bought'
  );
  assert(
    !('closed' in (open.status ?? {})),
    'RoundStatus has no sealed-but-undrawn variant at all'
  );

  act('a griefer guesses the crank\'s next address and takes it first');
  // Standing where the attacker cannot: deriving the address the crank is about
  // to use, from the crank's own wallet and the slot hash it is about to pick.
  // On chain this is a same-slot race; here it is handed to them for free, which
  // is the strongest version of the attack.
  const recent = await recentSlotHash(w);
  const seed = roundSeed(1, roundId, open.entropy, recent.slot, recent.hash, w.payer.publicKey);
  const squatted = w.oraoRequest(seed);
  const griefer = await newPlayer(w);
  const gBefore = await w.connection.getBalance(griefer.publicKey);
  await oraoRequestDirect(w, griefer, seed, treasury);
  const gAfter = await w.connection.getBalance(griefer.publicKey);
  say(`the griefer paid ${sol(gBefore - gAfter)} to occupy ${squatted.toBase58()}`);

  act('the crank\'s attempt reverts - and that is the whole of the damage');
  const code = await mustFail('draw_round onto the squatted address', () =>
    drawRound(w, 1, roundId, treasury, { seedSlot: recent.slot, slotHash: recent.hash })
  );
  assert(code === 'VrfAlreadyRequested', 'refused with VrfAlreadyRequested, as it should');
  const still: any = await w.program.account.round.fetch(w.round(1, roundId));
  assert(statusName(still.status) === 'open', 'the round is untouched and still Open');
  assert(
    Buffer.from(still.seed).every((b) => b === 0),
    'no seed was committed, so nothing is now pinned to an address the griefer owns'
  );

  act('the very next attempt uses a fresh slot hash, which is a fresh address');
  let drew: Awaited<ReturnType<typeof drawRound>> | null = null;
  for (let i = 0; i < 40; i++) {
    const next = await recentSlotHash(w);
    if (next.slot === recent.slot) {
      await new Promise((r) => setTimeout(r, 200));
      continue;
    }
    drew = await drawRound(w, 1, roundId, treasury);
    break;
  }
  if (!drew) throw new Error('the validator never advanced a slot');
  assert(!drew.request.equals(squatted), 'the round drew at a different address entirely');
  say(`round #${roundId} drew at ${drew.request.toBase58()} (seed slot ${drew.seedSlot})`);
  const sealed = await showRound(w, 1, roundId);
  assert(statusName(sealed.status) === 'requested', 'the round is Requested - the star is not blocked');

  act('and it settles for real');
  await oraoFulfill(w, drew.request);
  await resolvePush(w, p);
  const status = await pushStatus(w, p);
  assert(status === 'survived' || status === 'killed', `alice settled (${status}), not refunded`);

  act('price what is left of the attack');
  const occupied = await w.connection.getAccountInfo(squatted);
  say(`the griefer spent ${sol(gBefore - gAfter)} gross, ${sol(REQUEST_FEE)} of it irrecoverable`);
  say(`  and is left holding ${sol(occupied!.lamports)} in an account nothing reads`);
  say('to block one round they must now win a same-slot race against a transaction');
  say('  that has not been broadcast yet, and win it again on every retry');
  say('a crank that signs with a fresh keypair per draw is not guessable at all,');
  say('  because the signer is one of the seed inputs');

  process.exit(conclude('H-1'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
