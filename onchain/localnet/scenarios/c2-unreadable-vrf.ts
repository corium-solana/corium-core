/**
 * C-2 - a draw is only ever a signed callback, and only ever lands once.
 *
 * The original finding was about ORAO: `expire_round` called
 * `vrf::read_fulfilled(..)?` purely to confirm the draw had *not* landed, and
 * that helper returned `Err` - not `Ok(None)` - for a wrong owner, a wrong
 * account discriminator, an unknown enum tag, a short account, or a seed
 * mismatch. The `?` propagated all five, so a round whose ORAO account could
 * not be read could never be voided, and the settle path read the same account
 * and failed the same way. Both exits closed at once, and an oracle that
 * answered wrongly - or shipped an upgrade moving the layout this program
 * hardcoded - took the players' escrow with it.
 *
 * The MagicBlock migration deleted that entire failure class rather than fixing
 * it. Nothing parses a foreign account any more: the oracle pushes the draw
 * into `consume_randomness`, so there is no layout to misread and no
 * "unreadable versus unanswered" question for `expire_round` to get wrong. The
 * four damage shapes this scenario used to construct are no longer
 * constructible.
 *
 * What replaced them is a callback, and that is what this now pins. The whole
 * of the draw's integrity rests on one signature check, so each act below is
 * one way of trying to get a draw onto a round without it - plus the silent
 * oracle, which is the one leg of the original scenario that survives intact,
 * because surviving a silent oracle was always the design intent.
 */

import { Keypair } from '@solana/web3.js';

import {
  ROUND_EXPIRY_SLOTS,
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  consumeRandomnessDirect,
  expireRound,
  head,
  mustFail,
  newPlayer,
  requestPush,
  resolvePush,
  say,
  sealAndDraw,
  showRound,
  sol,
  statusName,
  vrfFulfill,
  vrfFulfillUnpinned,
  waitUntilSlot,
} from '../lib';

const STAKE = 40_000_000; // 0.04 SOL - one push clears the draw-cost bar

async function main() {
  head('C-2: a draw is only ever a signed callback');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  // ------------------------------------------------------------------ forgery
  act('a stranger calls consume_randomness directly, holding its own keypair');
  const alice = await newPlayer(w);
  const pAlice = await requestPush(w, 1, STAKE, alice);
  const seedA = await sealAndDraw(w, 1, pAlice.roundId, { fulfill: false });
  const forged = await mustFail('a self-signed draw', () =>
    consumeRandomnessDirect(w, 1, pAlice.roundId, Buffer.alloc(32, 0x42))
  );
  assert(
    forged === 'InvalidVrfCallbackIdentity' || forged === 'ConstraintAddress',
    `a draw not signed by the VRF identity is refused (${forged})`
  );
  const stillRequested = await showRound(w, 1, pAlice.roundId);
  assert(
    statusName(stillRequested.status) === 'requested',
    'the round is untouched and still waiting on its real callback'
  );

  act('and the real callback still works on that same round');
  await vrfFulfill(w, 1, pAlice.roundId, seedA);
  const drawn = await showRound(w, 1, pAlice.roundId);
  assert(statusName(drawn.status) === 'drawn', 'the oracle-signed draw lands');
  assert(
    Buffer.from(drawn.randomness).some((b) => b !== 0),
    'and the round now carries a non-zero draw'
  );

  // ------------------------------------------------------------------- replay
  act('the oracle delivers a second, different draw for the same round');
  const replay = await mustFail('a second draw', () =>
    vrfFulfillUnpinned(w, w.round(1, pAlice.roundId), Buffer.alloc(32, 0x99))
  );
  assert(replay === 'RoundNotRequested', `a landed draw cannot be rewritten (${replay})`);
  const after: any = await w.program.account.round.fetch(w.round(1, pAlice.roundId));
  assert(
    Buffer.from(after.randomness).equals(Buffer.from(drawn.randomness)),
    'the first draw is the one that stands'
  );
  await resolvePush(w, pAlice);

  // --------------------------------------------------------------- zero draw
  act('the oracle delivers an all-zero draw, which reads as "no draw at all"');
  const bob = await newPlayer(w);
  const pBob = await requestPush(w, 1, STAKE, bob);
  await sealAndDraw(w, 1, pBob.roundId, { fulfill: false });
  const zero = await mustFail('an all-zero draw', () =>
    vrfFulfillUnpinned(w, w.round(1, pBob.roundId), Buffer.alloc(32, 0))
  );
  assert(
    zero === 'ZeroRandomness',
    `a zero draw is refused rather than stored as Drawn (${zero})`
  );

  // ----------------------------------------------------- the silent oracle
  act('round C (control): the oracle simply never answers - the case the design is built for');
  const carol = await newPlayer(w);
  const pCarol = await requestPush(w, 1, STAKE, carol);
  await sealAndDraw(w, 1, pCarol.roundId, { fulfill: false });
  say(`push_id=${pCarol.pushId} is sealed and paid for, and nothing is coming`);

  for (const p of [pBob, pCarol]) {
    const acc: any = await w.program.account.round.fetch(w.round(1, p.roundId));
    await waitUntilSlot(
      w,
      Number(acc.requestedSlot) + ROUND_EXPIRY_SLOTS,
      `round #${p.roundId} expiry`
    );
  }

  act('both unanswered rounds void, because unanswered is not the same as landed');
  for (const p of [pBob, pCarol]) {
    await expireRound(w, 1, p.roundId);
    const acc = await showRound(w, 1, p.roundId);
    assert(statusName(acc.status) === 'expired', `round #${p.roundId} voided`);
  }

  // ------------------------------------------------------- the late callback
  act('the oracle wakes up and answers a round that already voided');
  const late = await mustFail('a post-expiry draw', () =>
    vrfFulfillUnpinned(w, w.round(1, pCarol.roundId), Buffer.alloc(32, 0x77))
  );
  assert(
    late === 'RoundNotRequested',
    `an expired round cannot be revived into a settlement (${late})`
  );

  act('and every member of a voided round refunds in full');
  let returned = 0;
  for (const p of [pBob, pCarol]) {
    const before = await w.connection.getBalance(p.player.publicKey);
    await resolvePush(w, p);
    const got = (await w.connection.getBalance(p.player.publicKey)) - before;
    returned += got;
    say(`round #${p.roundId}'s member got ${sol(got)} back`);
    assert(got >= STAKE - 10_000, `round #${p.roundId}: stake returned`);
  }
  say(`total returned ${sol(returned)}`);

  const cfg: any = await w.program.account.config.fetch(w.config);
  assert(Number(cfg.pendingLiability) === 0, 'no escrow is left in the vault');
  process.exit(conclude('C-2'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
