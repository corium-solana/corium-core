/**
 * Move accrued protocol revenue to the treasury frozen at initialize.
 *
 * Anyone can crank this, but only down to the draw float floor - the same balance
 * is what `draw_round` reimburses cranks for randomness out of, so a stranger
 * cannot leave the game with nothing to buy draws with. Running this from the
 * treasury's own wallet lifts the floor, because deciding to stop paying for
 * randomness is the house's decision to make.
 *
 *   yarn withdraw-fees                # everything collectable
 *   yarn withdraw-fees --amount 0.01
 */

import {
  BN,
  LAMPORTS_PER_SOL,
  SystemProgram,
  context,
  die,
  parseArgs,
  printSignature,
  sol,
} from './lib';

/** Mirrors `constants::DRAW_FLOAT_FLOOR`. */
const DRAW_FLOAT_FLOOR = new BN(LAMPORTS_PER_SOL / 20);

async function main() {
  const args = parseArgs();
  const { program, connection, wallet, config, vault } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die('Config not found.');

  const accrued = new BN(c.protocolAccrued);
  const isTreasury = wallet.publicKey.equals(c.treasury);
  const floor = isTreasury ? new BN(0) : DRAW_FLOAT_FLOOR;
  const collectable = BN.max(accrued.sub(floor), new BN(0));

  const amount = args.amount
    ? new BN(Math.round(Number(args.amount) * LAMPORTS_PER_SOL))
    : collectable;

  const vaultBalance = await connection.getBalance(vault);
  console.log(`vault            ${sol(vaultBalance)}`);
  console.log(`  pending escrow ${sol(c.pendingLiability)}`);
  console.log(`  prizes owed    ${sol(c.prizeLiability)}`);
  console.log(`accrued fees     ${sol(accrued)}`);
  console.log(
    `draw float floor ${sol(floor)}${isTreasury ? ' (lifted - signing as the treasury)' : ''}`
  );

  if (amount.isZero()) {
    die(
      accrued.isZero()
        ? 'Nothing accrued yet.'
        : `Only ${sol(accrued)} accrued, all of it inside the draw float floor. ` +
            'Run this from the treasury wallet to take it anyway.'
    );
  }
  if (amount.gt(accrued)) die(`Only ${sol(accrued)} accrued.`);
  if (amount.gt(collectable)) {
    die(
      `Only ${sol(collectable)} is collectable - the rest is the draw float. ` +
        'Run this from the treasury wallet to take it anyway.'
    );
  }

  console.log(`withdrawing      ${sol(amount)} -> ${c.treasury.toBase58()}\n`);

  const sig = await program.methods
    .withdrawProtocolFees(amount)
    .accountsPartial({
      crank: wallet.publicKey,
      config,
      vault,
      treasury: c.treasury,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  printSignature('withdraw_protocol_fees', sig);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
