/**
 * Drive rounds forward, then settle or cancel pending pushes. Permissionless -
 * the signer here is just paying transaction fees, and does not need to be the
 * pusher.
 *
 *   yarn resolve                       # advance rounds, then settle what is ready
 *   yarn resolve --push <pubkey>       # one specific push
 *   yarn resolve --close               # also reclaim rent on finished pushes
 *   yarn resolve --no-rounds           # settle only, leave sealing to another crank
 *
 * A push cannot settle until its round has been sealed and drawn, so this does
 * both by default. Ordering is by `settle_cursor`, which counts refunds too -
 * a voided round drains rather than wedging the queue behind it.
 */

import { PublicKey } from '@solana/web3.js';

import { advanceRounds } from '../../shared/chain/crank.js';
import { resolvePush } from '../../shared/chain/actions.js';
import { errSummary } from '../../shared/chain/send.js';

import {
  context,
  enumName,
  isStarFinished,
  parseArgs,
  printSignature,
  roundPda,
  sol,
  starPda,
} from './lib';
import { historyEnabled, recordPushClosed, attachRentToSettle } from './history';

export async function resolveAll(
  ctx: ReturnType<typeof context>,
  opts: {
    only?: PublicKey;
    close?: boolean;
    closeLimit?: number;
    quiet?: boolean;
    rounds?: boolean;
  } = {}
): Promise<{
  resolved: number;
  waiting: number;
  pendingRandomness: PublicKey[];
  sealed: number;
  drawn: number;
  voided: number;
  thin: number;
}> {
  const { program, programId, connection } = ctx;
  const log = (...a: any[]) => {
    if (!opts.quiet) console.log(...a);
  };

  // Sealing and drawing is what unblocks settlement, so it goes first. A round
  // that nobody seals is a round whose members cannot move.
  let sealed = 0;
  let drawn = 0;
  let voided = 0;
  let thin = 0;
  let watching: PublicKey[] = [];
  if (opts.rounds !== false) {
    const r = await advanceRounds(ctx as any, {
      onSealed: ({ round, signature }: any) =>
        printSignature(
          `sealed    round #${round.account.roundId} star #${round.account.starId} (${round.account.memberCount} members)`,
          signature
        ),
      onDrawn: ({ round, signature }: any) =>
        printSignature(
          `drew      round #${round.account.roundId} star #${round.account.starId}`,
          signature
        ),
      onVoided: ({ round, signature }: any) =>
        printSignature(
          `VOIDED    round #${round.account.roundId} star #${round.account.starId} - members can refund`,
          signature
        ),
      // Not a problem: the batch is ready by the clock but has not raked enough
      // to pay for a draw, so it stays open and keeps filling.
      onWaiting: ({ round, stake, needed, members }: any) =>
        log(
          `filling   round #${round.account.roundId} star #${round.account.starId} - ` +
            `${members} member(s), ${sol(stake)} of ${sol(needed)} needed for a draw`
        ),
      onError: (err: any, round: any) =>
        console.error(`round #${round.account.roundId}: ${errSummary(err)}`),
    });
    sealed = r.sealed;
    drawn = r.drawn;
    voided = r.voided;
    thin = r.thin;
    watching = r.watching;
  }

  let pushes = await program.account.pendingPush.all();
  if (opts.only) {
    pushes = pushes.filter((p) => p.publicKey.equals(opts.only!));
  }

  const pending = pushes.filter((p) => enumName(p.account.status) === 'pending');
  const settled = new Set<string>();
  let resolved = 0;
  let waiting = 0;

  // Star status is the cheap discriminator between "settle" and "refund", and
  // it is worth caching: a dead star usually has several stale pushes queued.
  const starCache = new Map<string, any>();
  const loadStar = async (starId: any) => {
    const key = starId.toString();
    if (!starCache.has(key)) {
      starCache.set(key, await program.account.star.fetch(starPda(programId, key)));
    }
    return starCache.get(key);
  };

  const roundCache = new Map<string, any>();
  const loadRound = async (starId: any, roundId: any) => {
    const key = `${starId}:${roundId}`;
    if (!roundCache.has(key)) {
      const pk = roundPda(programId, starId.toString(), roundId.toString());
      roundCache.set(key, await program.account.round.fetch(pk).catch(() => null));
    }
    return roundCache.get(key);
  };

  /**
   * A round's draw as hex, for the history record. Empty until the callback has
   * landed, which is also every refund path - those settle without a draw at
   * all, so there is genuinely nothing to record.
   */
  const drawHex = (round: any): string => {
    const draw = round?.randomness;
    if (!draw) return '';
    const bytes = Buffer.from(draw);
    return bytes.some((b) => b !== 0) ? bytes.toString('hex') : '';
  };

  const settleOne = async (p: (typeof pending)[number]) => {
    const a = p.account;
    const round = await loadRound(a.starId, a.roundId);
    const { signature: sig } = await resolvePush(ctx as any, {
      push: p.publicKey,
      account: a,
    });

    const after = await program.account.pendingPush.fetch(p.publicKey);
    const outcome = enumName(after.status);
    printSignature(
      `${outcome.padEnd(9)} push #${a.pushId} star #${a.starId} round #${a.roundId} ${sol(a.amount)}`,
      sig
    );
    if (outcome === 'killed') {
      console.log(`  *** STAR #${a.starId} DESTROYED by ${a.player.toBase58()} ***`);
    } else if (outcome === 'cancelled') {
      console.log(`  refunded ${sol(a.amount)} to ${a.player.toBase58()}`);
    }
    resolved++;
    settled.add(p.publicKey.toBase58());
    starCache.delete(a.starId.toString());
    if (historyEnabled()) {
      try {
        await attachRentToSettle(programId.toBase58(), p.publicKey.toBase58(), {
          randomness: drawHex(round),
        });
      } catch (e: any) {
        console.warn(`rent record failed push #${a.pushId}: ${e.message ?? e}`);
      }
    }
  };

  /**
   * Can this push move? A finished star and a voided round both refund without
   * needing any randomness at all; everything else has to wait for the draw.
   */
  const readiness = async (
    a: any
  ): Promise<{ ready: boolean; why: string }> => {
    const round = await loadRound(a.starId, a.roundId);
    const status = enumName(round?.status);
    if (!round) return { ready: false, why: 'round account missing' };
    if (status === 'expired') return { ready: true, why: '' };
    if (status === 'open') return { ready: false, why: 'round open, draw not bought yet' };
    if (status === 'requested') {
      return { ready: false, why: 'draw bought, waiting on the oracle callback' };
    }
    if (status !== 'drawn') return { ready: false, why: `round ${status}, no draw yet` };
    return { ready: true, why: '' };
  };

  // `--push` is explicit: try that account even if it is not the queue head.
  // Live out-of-order will fail on-chain with PushOutOfOrder.
  if (opts.only) {
    for (const p of pending) {
      const a = p.account;
      const star = await loadStar(a.starId);
      if (!isStarFinished(star.status)) {
        const { ready, why } = await readiness(a);
        if (!ready) {
          log(`waiting  push #${a.pushId} (star #${a.starId}) - ${why}`);
          waiting++;
          continue;
        }
      }
      try {
        await settleOne(p);
      } catch (e: any) {
        console.error(`failed   push #${a.pushId} (star #${a.starId}): ${errSummary(e)}`);
      }
    }
  } else {
    const byStar = new Map<string, typeof pending>();
    for (const p of pending) {
      const key = p.account.starId.toString();
      if (!byStar.has(key)) byStar.set(key, []);
      byStar.get(key)!.push(p);
    }

    for (const group of byStar.values()) {
      group.sort((a, b) => Number(a.account.pushId) - Number(b.account.pushId));
      const star = await loadStar(group[0].account.starId);

      // A finished star owes everyone their stake back, in any order.
      if (isStarFinished(star.status)) {
        for (const p of group) {
          try {
            await settleOne(p);
          } catch (e: any) {
            console.error(
              `failed   push #${p.account.pushId} (star #${p.account.starId}): ${errSummary(e)}`
            );
          }
        }
        continue;
      }

      let expected = Number(star.settleCursor);
      for (const p of group) {
        const a = p.account;
        const id = Number(a.pushId);
        if (id < expected) continue;
        if (id > expected) {
          log(`waiting  push #${a.pushId} (star #${a.starId}) - earlier push still pending`);
          waiting++;
          break;
        }
        const { ready, why } = await readiness(a);
        if (!ready) {
          log(`waiting  push #${a.pushId} (star #${a.starId}) - ${why}`);
          waiting++;
          break;
        }
        try {
          await settleOne(p);
          expected += 1;
        } catch (e: any) {
          console.error(`failed   push #${a.pushId} (star #${a.starId}): ${errSummary(e)}`);
          break;
        }
      }
    }
  }

  if (opts.close) {
    const finished = (await program.account.pendingPush.all()).filter(
      (p) => enumName(p.account.status) !== 'pending'
    );
    const cap = opts.closeLimit ?? finished.length;
    for (const p of finished.slice(0, cap)) {
      try {
        const info = await connection.getAccountInfo(p.publicKey);
        const lamports = info?.lamports ?? 0;
        const sig = await program.methods
          .closePush()
          .accountsPartial({
            pendingPush: p.publicKey,
            playerWallet: p.account.player,
          })
          .rpc();
        printSignature(`closed    push #${p.account.pushId}`, sig);
        if (historyEnabled()) {
          const round = await loadRound(p.account.starId, p.account.roundId);
          await recordPushClosed(programId.toBase58(), {
            signature: sig,
            push: p.publicKey.toBase58(),
            player: p.account.player.toBase58(),
            starId: p.account.starId.toString(),
            pushId: p.account.pushId.toString(),
            lamports,
            randomness: drawHex(round),
          });
        }
      } catch (e: any) {
        console.error(`close failed ${p.publicKey.toBase58()}: ${errSummary(e)}`);
      }
    }
  }

  // Anything still pending is waiting on its round's draw, so the caller should
  // watch the *round* - one callback now unblocks a whole batch.
  //
  // There is no separate oracle account to subscribe to any more: the VRF
  // program's callback writes the draw straight onto the round, so the round is
  // both the thing being waited on and the account that changes when the wait
  // ends.
  const pendingRandomness: PublicKey[] = [];
  const seen = new Set<string>();
  const watch = (address: PublicKey) => {
    const k = address.toBase58();
    if (seen.has(k)) return;
    seen.add(k);
    pendingRandomness.push(address);
  };
  for (const address of watching) watch(address);
  for (const p of pending) {
    if (settled.has(p.publicKey.toBase58())) continue;
    watch(roundPda(programId, p.account.starId, p.account.roundId));
  }

  return { resolved, waiting, pendingRandomness, sealed, drawn, voided, thin };
}

async function main() {
  const args = parseArgs();
  const ctx = context(args);
  const only = args.push ? new PublicKey(args.push as string) : undefined;

  const { resolved, waiting, sealed, drawn, voided, thin } = await resolveAll(ctx, {
    only,
    close: Boolean(args.close),
    rounds: !args['no-rounds'],
  });

  console.log(
    `\n${sealed} sealed, ${drawn} drawn, ${voided} voided, ${resolved} resolved, ` +
      `${waiting} still waiting on randomness, ${thin} still filling.`
  );
}

if (require.main === module) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}
