/**
 * Collect a dead star's jackpot. Must be signed by the Star Killer.
 *
 *   yarn claim-prize --star 1
 *   yarn claim-prize --star 1 --wallet ./wallets/bob.json
 */

import {
  SystemProgram,
  context,
  die,
  enumName,
  parseArgs,
  playerPda,
  printSignature,
  sol,
  starPda,
} from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, connection, wallet, config, vault } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die('Config not found.');

  const starId = args.star ? BigInt(args.star as string) : BigInt(c.currentStarId.toString());
  const star = starPda(programId, starId);
  const s = await program.account.star.fetch(star).catch(() => null);
  if (!s) die(`Star #${starId} not found.`);

  if (enumName(s.status) !== 'dead') die(`Star #${starId} is still alive.`);
  if (s.prizeClaimed) die(`Star #${starId}'s prize has already been claimed.`);
  if (!s.killer.equals(wallet.publicKey)) {
    die(
      `Only the Star Killer can claim.\n  killer: ${s.killer.toBase58()}\n  you:    ${wallet.publicKey.toBase58()}\n` +
        `  Retry with --wallet pointing at the killer's keypair.`
    );
  }

  const before = await connection.getBalance(wallet.publicKey);

  const sig = await program.methods
    .claimPrize()
    .accountsPartial({
      winner: wallet.publicKey,
      config,
      vault,
      star,
      playerStats: playerPda(programId, wallet.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  printSignature(`claim_prize star #${starId}`, sig);

  const after = await connection.getBalance(wallet.publicKey);
  console.log(`\nprize        ${sol(s.finalPrize)}`);
  console.log(`balance      ${sol(before)} -> ${sol(after)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
