/**
 * Ground truth for the oracle budget: read the actual lamport flows out of
 * real `request_push_vrf` and ORAO `fulfill` transactions on this cluster.
 *
 * Derived numbers keep disagreeing with each other (the docs' size formula,
 * getMinimumBalanceForRentExemption, and what fulfilled accounts actually
 * hold), so this trusts nothing but transaction metadata.
 *
 * Run: yarn tsx scripts/measure-orao-tx.ts [--limit 40]
 */
import { PublicKey } from '@solana/web3.js';

import { ORAO_VRF_PROGRAM_ID, context, parseArgs } from './lib';

const sol = (n: number) => (n / 1e9).toFixed(9);

async function main() {
  const args = parseArgs();
  const { connection, programId } = context(args);
  const limit = Number((args as any).limit ?? 40);

  const sigs = await connection.getSignaturesForAddress(programId, { limit });
  console.log(`scanning ${sigs.length} recent transactions on ${programId.toBase58()}\n`);

  let found = 0;
  for (const s of sigs) {
    if (s.err) continue;
    const tx = await connection.getTransaction(s.signature, {
      maxSupportedTransactionVersion: 0,
      commitment: 'confirmed',
    });
    if (!tx?.meta) continue;
    const logs = tx.meta.logMessages ?? [];
    if (!logs.some((l) => l.includes('RequestPushVrf') || l.includes('request_push_vrf'))) continue;

    const keys = tx.transaction.message.getAccountKeys().staticAccountKeys;
    const pre = tx.meta.preBalances;
    const post = tx.meta.postBalances;

    console.log(`request_push_vrf  ${s.signature}`);
    console.log(`  fee ${sol(tx.meta.fee)}`);
    let randomness: PublicKey | null = null;
    keys.forEach((k, i) => {
      const d = post[i] - pre[i];
      if (d === 0) return;
      const owner = pre[i] === 0 && post[i] > 0 ? ' (created)' : '';
      if (owner) randomness = k;
      console.log(`  ${k.toBase58().padEnd(45)} ${d > 0 ? '+' : ''}${sol(d)}${owner}`);
    });

    // Now the fulfill transaction for that same randomness account.
    if (randomness) {
      const rsigs = await connection.getSignaturesForAddress(randomness, { limit: 10 });
      for (const rs of rsigs) {
        if (rs.signature === s.signature || rs.err) continue;
        const ftx = await connection.getTransaction(rs.signature, {
          maxSupportedTransactionVersion: 0,
          commitment: 'confirmed',
        });
        if (!ftx?.meta) continue;
        const fkeys = ftx.transaction.message.getAccountKeys().staticAccountKeys;
        if (!fkeys.some((k) => k.equals(ORAO_VRF_PROGRAM_ID))) continue;
        console.log(`  -> fulfill ${rs.signature}`);
        fkeys.forEach((k, i) => {
          const d = ftx.meta!.postBalances[i] - ftx.meta!.preBalances[i];
          if (d !== 0) console.log(`     ${k.toBase58().padEnd(45)} ${d > 0 ? '+' : ''}${sol(d)}`);
        });
        break;
      }
    }
    console.log();
    if (++found >= 3) break;
  }
  if (!found) console.log('no request_push_vrf transactions in that window; raise --limit');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
