/**
 * H-1 - nobody but soldust can buy a draw for a soldust round.
 *
 * The bug: `close_round` published `round.seed` and `round.randomness` on chain
 * in a *separate transaction* from `request_round_vrf`. Between the two, the
 * address the round had committed to was public knowledge and ORAO's
 * `request_v2` was permissionless, so a stranger could occupy that address
 * first - after which `request_round_vrf` failed its emptiness pre-check
 * forever and the round could never become `Requested`. Escrow was safe (the
 * round expired into refunds) but a star under this attack could never settle a
 * single push, for about 0.0005 SOL net per round blocked.
 *
 * The fix merged the two into `draw_round`, so the seed is decided and spent in
 * one transaction, and that part still holds - acts 1 and 2 below.
 *
 * The rest of the original scenario is no longer constructible, and by a much
 * stronger argument than the fix gave us. Under ORAO the defence was that the
 * squatter could not *guess* the address, because the seed was chosen and spent
 * atomically. Under MagicBlock there is no address to squat, and filing a
 * request at all requires signing for `PDA(["identity"], soldust)` - a key only
 * soldust can produce. The permissionless request that made H-1 possible simply
 * does not exist.
 *
 * So the squatting acts are replaced by the thing that supersedes them: a
 * stranger attempting, three ways, to file a request naming soldust as its
 * callback. That gate is worth testing directly, because it is also what stops
 * an outsider aiming a draw at a round of their choosing.
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
  pushStatus,
  requestPush,
  resolvePush,
  say,
  showRound,
  sol,
  stakeNeededForDraw,
  statusName,
  vrfFulfill,
  vrfRequestDirect,
  waitOutWindow,
} from '../lib';

async function main() {
  head('H-1: only soldust can buy a draw for a soldust round');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  const need = await stakeNeededForDraw(w);
  act('a player queues, and the round sits open with nothing published to front-run');
  const alice = await newPlayer(w);
  const p = await requestPush(w, 1, need, alice);
  const open = await showRound(w, 1, p.roundId);

  // The first half of the original fix, unchanged: there is no intermediate
  // state in which a seed is public but the draw has not been bought.
  assert(statusName(open.status) === 'open', 'the round is Open');
  assert(
    Buffer.from(open.seed).every((b) => b === 0),
    'and carries no seed at all - there is no sealed-but-undrawn state to read'
  );
  assert(
    Buffer.from(open.randomness).every((b) => b === 0),
    'and no draw'
  );

  await waitOutWindow(w, 1, p.roundId);

  // ------------------------------------------------- the gate that replaced it
  act('a stranger tries to file a request naming soldust as the callback');
  const griefer = await newPlayer(w, 5 * LAMPORTS_PER_SOL);
  const before = await w.connection.getBalance(griefer.publicKey);

  // Presented without a signature at all.
  const unsigned = await mustFail("soldust's identity, not signed", () =>
    vrfRequestDirect(w, griefer)
  );
  assert(
    unsigned === 'MissingRequiredSignature',
    `an unsigned identity is refused (${unsigned})`
  );

  // Presented as a key the stranger does hold, which is the best they can do -
  // and it does not derive from soldust, so the VRF program rejects it.
  const impostor = Keypair.generate();
  const wrongKey = await mustFail('a key the stranger actually holds', () =>
    vrfRequestDirect(w, griefer, { sign: impostor })
  );
  assert(
    wrongKey === 'InvalidSeeds',
    `an identity that does not derive from soldust is refused (${wrongKey})`
  );

  // Both attempts die in simulation, so they never even reach a block - the
  // stranger cannot buy a draw, and cannot spend soldust's money trying.
  const spent = before - (await w.connection.getBalance(griefer.publicKey));
  say(`the stranger is out ${sol(spent)} and has achieved nothing`);
  assert(spent < REQUEST_FEE, 'and never paid a request fee');

  act('the round is untouched, so the crank draws it normally');
  const stillOpen = await showRound(w, 1, p.roundId);
  assert(statusName(stillOpen.status) === 'open', 'the round never left Open');

  const { seed } = await drawRound(w, 1, p.roundId);
  const sealed = await showRound(w, 1, p.roundId);
  assert(statusName(sealed.status) === 'requested', 'and seals on the first attempt');
  assert(
    Buffer.from(sealed.seed).equals(seed),
    'committing the seed the crank derived'
  );

  act('the oracle answers and the push settles');
  await vrfFulfill(w, 1, p.roundId, seed);
  await resolvePush(w, p);
  const status = await pushStatus(w, p);
  assert(status !== 'pending', `the push settled (status=${status})`);

  const cfg: any = await w.program.account.config.fetch(w.config);
  assert(Number(cfg.pendingLiability) === 0, 'and no escrow is left behind');
  process.exit(conclude('H-1'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
