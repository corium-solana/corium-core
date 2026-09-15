/**
 * One-time setup: create `Config` + the vault, then birth star #1.
 *
 *   yarn initialize
 *   yarn initialize --skip-first-star
 *   yarn initialize --treasury <pubkey> --genesis-seed <64 hex chars>
 */

import * as crypto from 'crypto';

import {
  BN,
  PublicKey,
  SystemProgram,
  context,
  die,
  parseArgs,
  printSignature,
  starPda,
  tryFetch,
} from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, config, vault, wallet } = context(args);

  const treasury = args.treasury
    ? new PublicKey(args.treasury as string)
    : wallet.publicKey;

  const genesisSeed = args['genesis-seed']
    ? Buffer.from(args['genesis-seed'] as string, 'hex')
    : crypto.randomBytes(32);
  if (genesisSeed.length !== 32) {
    die('--genesis-seed must be exactly 64 hex characters (32 bytes)');
  }

  console.log(`program   ${programId.toBase58()}`);
  console.log(`payer     ${wallet.publicKey.toBase58()}  (no authority is recorded)`);
  console.log(`treasury  ${treasury.toBase58()}  (frozen; there is no instruction to move it)`);
  console.log(`config    ${config.toBase58()}`);
  console.log(`vault     ${vault.toBase58()}`);
  console.log(`genesis   ${genesisSeed.toString('hex')}`);
  console.log();

  const existing = await tryFetch(program, 'config', config);
  if (existing) {
    console.log('Config already exists, skipping initialize.');
  } else {
    const sig = await program.methods
      .initialize(Array.from(genesisSeed), treasury)
      .accountsPartial({
        payer: wallet.publicKey,
        config,
        vault,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
    printSignature('initialize', sig);
  }

  if (args['skip-first-star']) {
    console.log('\n--skip-first-star set; run `yarn create-next-star` style setup yourself.');
    return;
  }

  const current = await program.account.config.fetch(config);
  if (!new BN(current.currentStarId).isZero()) {
    console.log(`\nStar #${current.currentStarId} already exists.`);
    return;
  }

  const star = starPda(programId, 1);
  const sig = await program.methods
    .createFirstStar()
    .accountsPartial({
      payer: wallet.publicKey,
      config,
      star,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  printSignature('create_first_star', sig);
  console.log(`\nStar #1 -> ${star.toBase58()}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
