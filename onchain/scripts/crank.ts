/**
 * Keep resolving forever, and keep Supabase history caught up.
 *
 *   yarn crank --wallet wallets/crank.json --interval 4 --auto-next-star
 *
 * Wakes on soldust logs (new push → maybe seal a round) and on each round's
 * randomness account (ORAO fulfill → resolve the batch). A round seals on a
 * slot count rather than an event, so `--interval` has to stay shorter than
 * ROUND_WINDOW_SLOTS or every round would be sealed late; it also covers
 * dropped websocket events, closes, and next-star.
 *
 * Live clients still read the chain for the current star and onLogs for new
 * pushes. This process is what fills the archive so browsers do not backfill.
 */

import { EventParser } from '@coral-xyz/anchor';
import { PublicKey, SystemProgram } from '@solana/web3.js';

import {
  BN,
  context,
  feedPda,
  isStarClosed,
  parseArgs,
  printSignature,
  stallCountdown,
  starPda,
} from './lib';
import { errSummary } from '../../shared/chain/send.js';
import { historyCluster } from '../../shared/chain/config.js';
import { resolveAll } from './resolve';
import { backfillRents, catchUpHistory, encodeEvent, historyEnabled, ingestEncoded } from './history';

const WAKE_DEBOUNCE_MS = 80;

async function maybeCreateNextStar(ctx: ReturnType<typeof context>) {
  const { program, programId, wallet, config } = ctx;
  const c = await program.account.config.fetch(config);
  const currentId = BigInt(c.currentStarId.toString());
  if (currentId === 0n) return;

  const prevStar = starPda(programId, currentId);
  const s = await program.account.star.fetch(prevStar);
  if (!isStarClosed(s) || s.nextStarCreated) return;

  const newId = currentId + 1n;
  const sig = await program.methods
    .createNextStar(new BN(newId.toString()))
    .accountsPartial({
      payer: wallet.publicKey,
      config,
      prevStar,
      nextStar: starPda(programId, newId),
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  printSignature(`create_next_star #${newId}`, sig);
}

/**
 * Collapse the current star if it has gone its whole stall window without
 * gaining mass, so feeders can take their money out and play can move on.
 *
 * Permissionless and pays nobody, so it does not matter who submits it - but
 * somebody has to, and if the reason the star went quiet is that ORAO died, this
 * process is the only thing still running. A minute of cushion because the check
 * reads the local clock and the program reads the cluster's.
 */
async function maybeCollapseStalledStar(ctx: ReturnType<typeof context>) {
  const { program, programId, wallet, config } = ctx;
  const c = await program.account.config.fetch(config);
  const currentId = BigInt(c.currentStarId.toString());
  if (currentId === 0n) return;

  const star = starPda(programId, currentId);
  const left = stallCountdown(await program.account.star.fetch(star));
  if (left === null || left > -60) return;

  try {
    const sig = await program.methods
      .collapseStalledStar()
      .accountsPartial({
        cranker: wallet.publicKey,
        config,
        star,
        starFeed: feedPda(programId, currentId),
      })
      .rpc();
    printSignature(`collapse_stalled_star #${currentId}`, sig);
  } catch (e: any) {
    // All of these mean "not our turn after all": mass or a push landed between
    // the read and the send, someone else got there first, or nobody ever fed
    // this star, so it has no `StarFeed` and no player money to free.
    const text = errSummary(e);
    if (!/StarNotStalled|StarQueueNotEmpty|StarNotAlive|AccountNotInitialized/i.test(text)) {
      console.warn(`collapse_stalled_star #${currentId} failed: ${text}`);
    }
  }
}

function listenAndStore(ctx: ReturnType<typeof context>, onLogs: () => void) {
  const parser = new EventParser(ctx.programId, ctx.program.coder);
  const programId = ctx.programId.toBase58();
  const index = historyEnabled();
  const id = ctx.connection.onLogs(
    ctx.programId,
    (logs) => {
      onLogs();
      if (!index || logs.err) return;
      const rows = [];
      try {
        for (const event of parser.parseLogs(logs.logs)) {
          rows.push(
            encodeEvent(programId, event.name, event.data, {
              signature: logs.signature,
              slot: 0,
              blockTime: Math.floor(Date.now() / 1000),
            })
          );
        }
      } catch {
        return;
      }
      if (rows.length) ingestEncoded(rows).catch((err) => console.warn('index ingest', err));
    },
    'confirmed'
  );
  return () => {
    ctx.connection.removeOnLogsListener(id).catch(() => {});
  };
}

async function syncRandomnessWatches(
  ctx: ReturnType<typeof context>,
  keys: PublicKey[],
  watches: Map<string, number>,
  wake: (reason: string) => void
) {
  const want = new Set(keys.map((k) => k.toBase58()));
  for (const [k, id] of watches) {
    if (want.has(k)) continue;
    ctx.connection.removeAccountChangeListener(id).catch(() => {});
    watches.delete(k);
  }
  for (const key of keys) {
    const k = key.toBase58();
    if (watches.has(k)) continue;
    const id = ctx.connection.onAccountChange(key, () => wake('vrf'), 'confirmed');
    watches.set(k, id);
  }
}

async function main() {
  const args = parseArgs();
  const ctx = context(args);
  const interval = Number(args.interval ?? 4) * 1000;
  const autoNext = Boolean(args['auto-next-star']);
  const index = args.index === undefined ? historyEnabled() : Boolean(args.index);

  console.log(
    `Cranking ${ctx.programId.toBase58()} as ${ctx.wallet.publicKey.toBase58()} (event-driven, poll ${interval / 1000}s).`
  );
  if (index) {
    console.log(
      `History → ${process.env.SUPABASE_URL || process.env.VITE_SUPABASE_URL} [${historyCluster()}]`
    );
  } else console.log('History off (no SUPABASE_SERVICE_ROLE_KEY).');
  console.log('Ctrl-C to stop.\n');

  let busy = false;
  let queued: string | null = null;
  let debounce: ReturnType<typeof setTimeout> | null = null;
  const watches = new Map<string, number>();

  const tick = async (reason: string) => {
    if (busy) {
      queued = reason;
      return;
    }
    busy = true;
    try {
      // Sweep closes / history on the poll and right after a settle so the
      // happy path stays a couple of RPCs.
      const close = reason !== 'soldust';
      const { resolved, waiting, pendingRandomness, sealed, drawn, voided, thin } =
        await resolveAll(ctx, {
          quiet: true,
          close,
          closeLimit: reason === 'poll' || reason === 'start' ? 16 : 4,
        });
      await syncRandomnessWatches(ctx, pendingRandomness, watches, wake);
      if (resolved || waiting || sealed || drawn || voided || thin) {
        console.log(
          `[${new Date().toISOString()}] ${reason} sealed ${sealed}, drew ${drawn}, voided ${voided}, resolved ${resolved}, waiting ${waiting}, filling ${thin}`
        );
      }
      // Day-scale, so the unhurried poll is the only place worth asking, and
      // before the successor check on purpose: a collapse closes the star, so the
      // same tick can go on to open the next one.
      if (reason === 'poll' || reason === 'start') {
        await maybeCollapseStalledStar(ctx);
      }
      if (autoNext && (resolved || reason === 'poll' || reason === 'start')) {
        await maybeCreateNextStar(ctx);
      }
      if (index && (resolved || reason === 'poll' || reason === 'start')) {
        const result = await catchUpHistory(ctx, { maxPages: reason === 'poll' ? 4 : 2 });
        if (result.stored) {
          console.log(
            `[${new Date().toISOString()}] indexed ${result.stored} complete=${result.oldestComplete}`
          );
        }
        try {
          const rents = await backfillRents(ctx, { limit: 12 });
          if (rents.closed) {
            console.log(`[${new Date().toISOString()}] rent links closed=${rents.closed}`);
          }
        } catch (err: any) {
          console.warn(`rent backfill failed: ${err.message ?? err}`);
        }
      }
    } catch (e: any) {
      console.error(`crank error (${reason}): ${errSummary(e)}`);
    } finally {
      busy = false;
      if (queued) {
        const next = queued;
        queued = null;
        void tick(next);
      }
    }
  };

  const wake = (reason: string) => {
    if (debounce) clearTimeout(debounce);
    debounce = setTimeout(() => {
      debounce = null;
      void tick(reason);
    }, WAKE_DEBOUNCE_MS);
  };

  const stopListen = listenAndStore(ctx, () => wake('soldust'));
  const poll = setInterval(() => wake('poll'), interval);

  const stop = () => {
    stopListen();
    clearInterval(poll);
    if (debounce) clearTimeout(debounce);
    for (const id of watches.values()) {
      ctx.connection.removeAccountChangeListener(id).catch(() => {});
    }
    watches.clear();
  };

  process.on('SIGINT', () => {
    stop();
    process.exit(0);
  });

  if (index) {
    try {
      const result = await catchUpHistory(ctx, { maxPages: 20 });
      console.log(`history catch-up stored=${result.stored} complete=${result.oldestComplete}`);
    } catch (e: any) {
      console.error(`history catch-up failed: ${e.message ?? e}`);
    }
  }

  await tick('start');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
