/**
 * M-4 (fixed) - a stranger can collect the rake, but not switch the oracle off.
 *
 * The bug: `withdraw_protocol_fees` was permissionless and had no floor, so
 * anyone could move the whole of `protocol_accrued` to the treasury whenever they
 * liked. No lamport went anywhere it should not - the destination is frozen at
 * `initialize` - but that balance is also the float `draw_round` reimburses cranks
 * out of. Swept to zero, every draw comes out of the cranker's own pocket: the
 * house crank keeps working and quietly eats the cost, and no third party has any
 * reason to run one, which is the property that makes the game live without the
 * house. Repeat it on a timer and cranking is unpaid work forever.
 *
 * The fix keeps collection permissionless above `DRAW_FLOAT_FLOOR` and lets only
 * the treasury, signing for itself, go below. The house never has to be online to
 * be paid, and it is the only party that can decide to stop buying randomness.
 */

import { Keypair } from '@solana/web3.js';

import {
  act,
  assert,
  bootstrap,
  conclude,
  connect,
  drawCost,
  head,
  mustFail,
  newPlayer,
  say,
  sol,
  withdrawFees,
  DRAW_FLOAT_FLOOR,
} from '../lib';

async function main() {
  head('M-4: the draw float survives a permissionless sweep');
  const w = await connect();
  // A keypair rather than a bare address, because one assertion needs the
  // treasury to actually sign.
  const treasury = await newPlayer(w);
  await bootstrap(w, treasury.publicKey);

  const before: any = await w.program.account.config.fetch(w.config);
  const accrued = Number(before.protocolAccrued);
  say(`accrued ${sol(accrued)}, floor ${sol(DRAW_FLOAT_FLOOR)}`);
  assert(accrued > DRAW_FLOAT_FLOOR, 'there is more accrued than the floor, so this is a real test');

  act('a stranger tries to take the lot');
  const code = await mustFail('sweep everything as a stranger', () =>
    withdrawFees(w, treasury.publicKey, accrued)
  );
  assert(code === 'WouldDrainDrawFloat', 'refused with WouldDrainDrawFloat');

  act('and tries again one lamport over the line');
  const overshoot = await mustFail('leave one lamport less than the floor', () =>
    withdrawFees(w, treasury.publicKey, accrued - DRAW_FLOAT_FLOOR + 1)
  );
  assert(overshoot === 'WouldDrainDrawFloat', 'refused at exactly one lamport too many');

  act('but everything above the floor is still anyone\'s to collect');
  const paidBefore = await w.connection.getBalance(treasury.publicKey);
  await withdrawFees(w, treasury.publicKey, accrued - DRAW_FLOAT_FLOOR);
  const paidAfter = await w.connection.getBalance(treasury.publicKey);
  assert(
    paidAfter - paidBefore === accrued - DRAW_FLOAT_FLOOR,
    `the treasury received ${sol(accrued - DRAW_FLOAT_FLOOR)} from a wallet that is not it`
  );
  const swept: any = await w.program.account.config.fetch(w.config);
  assert(
    Number(swept.protocolAccrued) === DRAW_FLOAT_FLOOR,
    'and exactly the floor is left behind to buy randomness with'
  );

  act('now the floor holds against everyone but the treasury');
  const dust = await mustFail('take one more lamport as a stranger', () =>
    withdrawFees(w, treasury.publicKey, 1)
  );
  assert(dust === 'WouldDrainDrawFloat', 'even one lamport is refused at the floor');

  act('the treasury signs for itself and takes the rest');
  await withdrawFees(w, treasury.publicKey, DRAW_FLOAT_FLOOR, { as: treasury });
  const emptied: any = await w.program.account.config.fetch(w.config);
  assert(Number(emptied.protocolAccrued) === 0, 'the float is the house\'s to close, and only the house\'s');

  act('what the floor is worth');
  const cost = await drawCost(w);
  say(
    `${sol(DRAW_FLOAT_FLOOR)} is ${Math.floor(DRAW_FLOAT_FLOOR / cost)} draws at ` +
      `${sol(cost)} each - enough that a star keeps`
  );
  say('  moving on third-party cranks while a human notices the rake is due');
  say('nobody gained a claim on the money: the destination never moved, and a');
  say('  stranger cranking a withdrawal still pays a fee to hand the house cash');

  process.exit(conclude('M-4'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
