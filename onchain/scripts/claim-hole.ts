/**
 * Collect an early-feed share from a collapsed star.
 *
 *   yarn claim-hole --star 1
 *   yarn claim-hole --star 1 --wallet ./wallets/bob.json
 */

import {
  SystemProgram,
  context,
  die,
  enumName,
  feedPda,
  feedSharePda,
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
  if (enumName(s.status) !== 'blackHole') die(`Star #${starId} is not a black hole.`);

  const sharePk = feedSharePda(programId, starId, wallet.publicKey);
  const share = await program.account.feedShare.fetch(sharePk).catch(() => null);
  if (!share || Number(share.amount.toString()) === 0) {
    die(`No hole ticket on star #${starId} for ${wallet.publicKey.toBase58()}.`);
  }
  if (share.claimed) die(`Hole share on star #${starId} already claimed.`);

  const feed = await program.account.starFeed.fetch(feedPda(programId, starId));
  const amount =
    (BigInt(share.amount.toString()) * BigInt(s.finalPrize.toString())) /
    BigInt(feed.earlyVolume.toString());

  const before = await connection.getBalance(wallet.publicKey);
  const sig = await program.methods
    .claimHoleShare()
    .accountsPartial({
      player: wallet.publicKey,
      config,
      vault,
      star,
      starFeed: feedPda(programId, starId),
      feedShare: sharePk,
      playerStats: playerPda(programId, wallet.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  printSignature(`claim_hole_share star #${starId}`, sig);
  const after = await connection.getBalance(wallet.publicKey);
  console.log(`\nshare        ${sol(amount)}`);
  console.log(`balance      ${sol(before)} -> ${sol(after)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
