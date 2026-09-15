/**
 * Birth the next star. Permissionless - any wallet can pay the rent.
 *
 *   yarn create-next-star
 */

import {
  BN,
  SystemProgram,
  context,
  die,
  isStarClosed,
  parseArgs,
  printSignature,
  starPda,
} from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, wallet, config } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die('Config not found. Run `yarn initialize`.');

  const currentId = BigInt(c.currentStarId.toString());
  if (currentId === 0n) die('No first star yet. Run `yarn initialize`.');

  const prevStar = starPda(programId, currentId);
  const s = await program.account.star.fetch(prevStar);

  if (!isStarClosed(s)) {
    die(`Star #${currentId} is still open. It has to die or commit 21 SOL first.`);
  }
  if (s.nextStarCreated) {
    die(`Star #${currentId} has already produced a successor.`);
  }
  if (Number(s.pendingPushes) > 0) {
    console.warn(
      `Star #${currentId} still has ${s.pendingPushes} unresolved push(es). ` +
        `The next star can be born anyway; the old queue keeps settling.`
    );
  }

  const newId = currentId + 1n;
  const nextStar = starPda(programId, newId);

  const sig = await program.methods
    .createNextStar(new BN(newId.toString()))
    .accountsPartial({
      payer: wallet.publicKey,
      config,
      prevStar,
      nextStar,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  printSignature(`create_next_star #${newId}`, sig);

  const created = await program.account.star.fetch(nextStar);
  console.log(`\nStar #${newId} ${nextStar.toBase58()}`);
  console.log(`visual seed ${Buffer.from(created.seed).toString('hex')}`);
  console.log(`(derived from star #${currentId}'s seed)`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
