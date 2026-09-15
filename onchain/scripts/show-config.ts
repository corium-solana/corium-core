/**
 * Print the global config: accounting, vault solvency, and the compiled
 * curve the program is actually running.
 *
 *   yarn show-config
 */

import { LOCKED_ECONOMICS, PUSH_STEP } from '../../shared/chain/economics.js';
import { fetchDrawCost } from '../../shared/chain/orao.js';

import { context, die, parseArgs, sol, stageName } from './lib';

async function main() {
  const args = parseArgs();
  const { program, connection, config, vault, programId } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die(`Config not found at ${config.toBase58()}. Run \`yarn initialize\`.`);

  const vaultLamports = BigInt(await connection.getBalance(vault));
  const pending = BigInt(c.pendingLiability.toString());
  const prizes = BigInt(c.prizeLiability.toString());
  const protocolFees = BigInt(c.protocolAccrued.toString());
  const nextReserve = BigInt(c.nextStarReserve.toString());
  const reserved = pending + prizes + nextReserve;

  console.log('== SOLDUST ==');
  console.log(`program            ${programId.toBase58()}`);
  console.log(`config             ${config.toBase58()}`);
  console.log(`treasury           ${c.treasury.toBase58()}`);
  console.log(`current star       #${c.currentStarId}`);
  console.log(`stars created      ${c.starsCreated}`);
  console.log(`genesis seed       ${Buffer.from(c.genesisSeed).toString('hex')}`);

  console.log('\n== vault ==');
  console.log(`address            ${vault.toBase58()}`);
  console.log(`balance            ${sol(vaultLamports)}`);
  console.log(`  pending escrow   ${sol(pending)}`);
  console.log(`  prizes owed      ${sol(prizes)}`);
  console.log(`  next-star reserve ${sol(nextReserve)}`);
  console.log(`  protocol fees    ${sol(protocolFees)}   (also the randomness float)`);
  console.log(`  reserved total   ${sol(reserved)}`);
  console.log(
    `solvent            ${vaultLamports >= reserved + protocolFees ? 'yes' : 'NO - INVESTIGATE'}`
  );

  // Not read from the account: Config carries no tunables. These are the
  // numbers compiled into the deployed binary.
  const e = LOCKED_ECONOMICS;
  console.log('\n== economics (compiled into the program; no instruction can change them) ==');
  console.log(`push step / floor  ${sol(PUSH_STEP)} - pushes are whole multiples of it`);
  console.log('max                = room left to the nursery cap or the hole cap');
  console.log('nova chance        = amount / (mass_before + amount), no cap');
  console.log(`                     so every push returns ${e.prizeBps / 100}% of its stake`);
  console.log(`split              prize ${e.prizeBps / 100}% / protocol ${e.protocolBps / 100}%`);
  console.log(`stardust           ${e.stardustPerSol} per SOL`);

  // The player never pays for this. It comes out of protocol_accrued, and one
  // draw covers every member of the round that bought it - so the per-push cost
  // falls as the batch fills.
  const draw = BigInt(await fetchDrawCost(connection));
  console.log('\n== randomness (house cost, not a player fee) ==');
  console.log(`one draw           ${sol(draw)} at ORAO's current price`);
  console.log(`  shared by 4      ${sol(draw / 4n)} per push`);
  console.log(`  shared by 24     ${sol(draw / 24n)} per push`);
  console.log(
    `break-even         ${sol((draw * 10_000n) / BigInt(e.protocolBps))} of round stake covers one draw at ${
      e.protocolBps / 100
    }%`
  );
  console.log("  enforced         close_round refuses a round below this and leaves it open");
  console.log('                   for more members, so no draw is ever bought at a loss');

  const star =
    BigInt(c.currentStarId.toString()) > 0n
      ? await program.account.star
          .fetch(
            (await import('./lib')).starPda(programId, BigInt(c.currentStarId.toString()))
          )
          .catch(() => null)
      : null;
  if (star) {
    console.log(`\n== stages frozen onto star #${c.currentStarId} at birth ==`);
    const l = star.lifecycle;
    for (let i = 0; i < l.stageCount; i++) {
      const s = l.stages[i];
      console.log(
        `  ${String(i).padEnd(2)} ${stageName(i).padEnd(14)} from ${sol(s.minMass).padStart(14)}   dust ${(s.stardustMultBps / 10000).toFixed(2)}×`
      );
    }
    console.log('  (odds are not in here - they are amount / mass_after)');
  }

  console.log('\n== lifetime ==');
  console.log(`pushes settled     ${c.totalPushesSettled}`);
  console.log(`volume             ${sol(c.totalVolume)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
