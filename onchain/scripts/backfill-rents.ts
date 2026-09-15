/**
 * One-shot: attach `close_push` signatures onto last-hit history.
 * The live crank only does a small page per tick, and only after deploy.
 *
 *   yarn tsx scripts/backfill-rents.ts
 */

import { context, parseArgs } from './lib';
import { backfillRents, historyEnabled } from './history';

async function main() {
  if (!historyEnabled()) {
    console.error('Set SUPABASE_URL and SUPABASE_SERVICE_ROLE_KEY in onchain/.env');
    process.exit(1);
  }
  const ctx = context(parseArgs());
  console.log(`Rent backfill ${ctx.programId.toBase58()}`);
  let offset = 0;
  let closed = 0;
  let scanned = 0;
  const page = 20;
  for (let round = 1; round <= 200; round++) {
    const result = await backfillRents(ctx, { limit: page, offset });
    closed += result.closed;
    scanned += result.scanned;
    console.log(
      `page ${round} offset=${offset} scanned=${result.scanned} closed+=${result.closed}`
    );
    offset += result.scanned;
    if (result.done) break;
  }
  console.log(`done scanned=${scanned} close links=${closed}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
