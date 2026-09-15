/**
 * One-way donation into protocol_accrued, which is what draws are bought from.
 *
 *   yarn fund-protocol                 # 0.1 SOL
 *   yarn fund-protocol --amount 0.05
 */

import { BN, LAMPORTS_PER_SOL, SystemProgram, context, parseArgs, printSignature, sol } from './lib';

async function main() {
  const args = parseArgs();
  const { program, wallet, config, vault } = context(args);
  const amountSol = Number(args.amount ?? 0.1);
  const lamports = new BN(Math.round(amountSol * LAMPORTS_PER_SOL));

  const before = await program.account.config.fetch(config);
  console.log(`payer     ${wallet.publicKey.toBase58()}`);
  console.log(`amount    ${sol(lamports)}`);
  console.log(`accrued   ${sol(before.protocolAccrued)} →`);

  const sig = await program.methods
    .fundProtocol(lamports)
    .accountsPartial({
      payer: wallet.publicKey,
      config,
      vault,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  const after = await program.account.config.fetch(config);
  printSignature('fund_protocol', sig);
  console.log(`accrued   ${sol(after.protocolAccrued)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
