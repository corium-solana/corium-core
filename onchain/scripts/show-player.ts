/**
 * Lifetime stats for a wallet. Defaults to the current signer.
 *
 *   yarn show-player
 *   yarn show-player --player <pubkey>
 *   yarn show-player --wallet ./wallets/bob.json
 *   yarn show-player --leaderboard
 */

import { PublicKey, context, parseArgs, playerPda, sol } from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, wallet, connection } = context(args);

  if (args.leaderboard) {
    const all = await program.account.player.all();
    all.sort((a, b) => Number(b.account.stardust) - Number(a.account.stardust));
    console.log('wallet                                       stardust  pushes  kills  pushed');
    for (const p of all) {
      const a = p.account;
      console.log(
        `${a.wallet.toBase58()}  ${String(a.stardust).padStart(8)}  ${String(
          a.successfulPushes
        ).padStart(6)}  ${String(a.starsKilled).padStart(5)}  ${sol(a.totalSolPushed)}`
      );
    }
    if (all.length === 0) console.log('(no players yet)');
    return;
  }

  const target = args.player
    ? new PublicKey(args.player as string)
    : wallet.publicKey;
  const address = playerPda(programId, target);
  const p = await program.account.player.fetch(address).catch(() => null);

  console.log(`wallet             ${target.toBase58()}`);
  console.log(`balance            ${sol(await connection.getBalance(target))}`);
  console.log(`player pda         ${address.toBase58()}`);

  if (!p) {
    console.log('\nNo player account yet - this wallet has never pushed.');
    return;
  }

  console.log(`\nSTARDUST           ${p.stardust}`);
  console.log(`pushes requested   ${p.requestedPushes}`);
  console.log(`pushes successful  ${p.successfulPushes}`);
  console.log(`pushes cancelled   ${p.cancelledPushes}`);
  console.log(`total pushed       ${sol(p.totalSolPushed)}`);
  console.log(`stars killed       ${p.starsKilled}`);
  console.log(`prizes won         ${sol(p.prizesWon)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
