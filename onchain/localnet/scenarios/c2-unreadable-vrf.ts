/**
 * C-2 (fixed) - an unreadable ORAO account must not be mistaken for a landed draw.
 *
 * The bug: `expire_round` called `vrf::read_fulfilled(..)?` purely to confirm the
 * draw had *not* landed. `read_fulfilled` returns `Err` - not `Ok(None)` - for a
 * wrong owner, a wrong account discriminator, an unknown enum tag, a short
 * account, or a seed mismatch. The `?` propagated all five, so a round whose ORAO
 * account could not be read could never be voided. The settle path reads the same
 * account and fails the same way, so both exits closed at once.
 *
 * This is precisely the "ORAO goes rip" case: an oracle that answers wrongly, or
 * ships a program upgrade that moves the layout this program hardcodes, took the
 * players' escrow with it. The design intends to survive an oracle that goes
 * *silent*, and it always did - round D below is the control that proves it.
 *
 * The fix treats an unreadable answer as no answer, which is safe in both
 * directions: the account is address-pinned to `round.randomness`, so nobody can
 * substitute a broken one to duck a roll, and the settle path refuses it anyway.
 * All four damage shapes below now void and refund.
 */

import { Keypair, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';

import {
  ROUND_EXPIRY_SLOTS,
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  expireRound,
  head,
  newPlayer,
  oraoCorrupt,
  requestPush,
  resolvePush,
  say,
  sealAndDraw,
  showRound,
  sol,
  statusName,
  waitUntilSlot,
} from '../lib';

const STAKE = 80_000_000; // 0.08 SOL - one push clears the draw-cost bar

/**
 * `read_fulfilled` short-circuits to `Ok(None)` on the pending tag, so damage to
 * the recorded seed only bites once a draw has landed. The `fulfilled` cases are
 * the realistic shape of an ORAO upgrade: the oracle answers, but writes the
 * account in a layout this program cannot parse.
 */
const CASES = [
  { mode: 0, fulfilled: false, expect: 'MalformedVrfAccount', what: 'in-flight request rewritten with a new account discriminator' },
  { mode: 1, fulfilled: false, expect: 'MalformedVrfAccount', what: 'in-flight request rewritten with an unknown enum tag' },
  { mode: 0, fulfilled: true, expect: 'MalformedVrfAccount', what: 'ORAO answers, but in a renamed account struct' },
  { mode: 2, fulfilled: true, expect: 'RandomnessSeedMismatch', what: 'ORAO answers with the wrong seed recorded' },
];

type Case = {
  label: string;
  roundId: number;
  push: Awaited<ReturnType<typeof requestPush>>;
  request: PublicKey;
  expect?: string;
  what: string;
};

async function main() {
  head('C-2: an unreadable ORAO account closes both exits');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  const rounds: Case[] = [];

  for (const [i, c] of CASES.entries()) {
    const label = 'ABCD'[i];
    act(`round ${label}: one player queues, the draw is bought, then - ${c.what}`);
    const player = await newPlayer(w);
    const push = await requestPush(w, 1, STAKE, player);
    const request = await sealAndDraw(w, 1, push.roundId, treasury, { fulfill: c.fulfilled });
    await oraoCorrupt(w, request, c.mode);
    say(`push_id=${push.pushId}, ${request.toBase58()} damaged (mode ${c.mode})`);
    rounds.push({ label, roundId: push.roundId, push, request, expect: c.expect, what: c.what });
  }

  act('round E (control): identical, except ORAO simply never answers');
  const dave = await newPlayer(w);
  const pDave = await requestPush(w, 1, STAKE, dave);
  const requestD = await sealAndDraw(w, 1, pDave.roundId, treasury, { fulfill: false });
  say(`push_id=${pDave.pushId}, randomness left Pending and untouched`);
  const control: Case = {
    label: 'E',
    roundId: pDave.roundId,
    push: pDave,
    request: requestD,
    what: 'a silent oracle - the case the design is built to survive',
  };

  act('wait out the expiry window for every round');
  for (const r of [...rounds, control]) {
    const acc: any = await w.program.account.round.fetch(w.round(1, r.roundId));
    await waitUntilSlot(w, Number(acc.requestedSlot) + ROUND_EXPIRY_SLOTS, `round ${r.label} expiry`);
  }

  act('every damaged round now voids, because unreadable is not the same as landed');
  for (const r of [...rounds, control]) {
    await expireRound(w, 1, r.roundId);
    const acc = await showRound(w, 1, r.roundId);
    assert(statusName(acc.status) === 'expired', `round ${r.label} voided - ${r.what}`);
  }

  act('and every member refunds in full, in queue order');
  let returned = 0;
  for (const r of [...rounds, control]) {
    const before = await w.connection.getBalance(r.push.player.publicKey);
    await resolvePush(w, r.push);
    const got = (await w.connection.getBalance(r.push.player.publicKey)) - before;
    returned += got;
    say(`round ${r.label}'s member got ${sol(got)} back`);
    assert(got >= STAKE - 10_000, `round ${r.label}: stake returned`);
  }
  const cfgFixed: any = await w.program.account.config.fetch(w.config);
  say(`total returned ${sol(returned)}`);
  assert(Number(cfgFixed.pendingLiability) === 0, 'no escrow is left in the vault');
  process.exit(conclude('C-2'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
