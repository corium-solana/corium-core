/**
 * One-shot history backfill. The crank also does this every loop.
 *   yarn tsx scripts/index-once.ts
 */

import { context, parseArgs } from './lib';
import { catchUpHistory, historyEnabled } from './history';

async function main() {
  if (!historyEnabled()) {
    console.error('Set SUPABASE_URL and SUPABASE_SERVICE_ROLE_KEY in onchain/.env');
    process.exit(1);
  }
  const ctx = context(parseArgs());
  console.log(`Indexing ${ctx.programId.toBase58()} …`);
  let complete = false;
  let rounds = 0;
  while (!complete && rounds < 80) {
    rounds += 1;
    const result = await catchUpHistory(ctx, {
      maxPages: 10,
      onProgress: (p) => console.log(p),
    });
    complete = !!result.oldestComplete;
    console.log(
      `round ${rounds} stored=${result.stored} added=${result.added} complete=${complete}`
    );
    if (!result.stored && complete) break;
    if (!result.stored && !complete) {
      /* still walking; empty page means done */
      break;
    }
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
