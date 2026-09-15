/**
 * Escrow SOL at the current star and join its open round. One signature, and
 * the player pays nothing towards randomness - the round buys one draw for
 * every member out of the house cut, which the crank does separately.
 *
 *   yarn push
 *   yarn push --amount 0.1
 *   yarn push --count 3 --wallet ./wallets/bob.json
 *   yarn push --star 2                 # fails unless #2 is the current star
 *   yarn push --amount 0.012 --raw     # skip the snap, let the program reject it
 */

import * as crypto from 'crypto';

import {
  BN,
  LAMPORTS_PER_SOL,
  SystemProgram,
  context,
  die,
  feedPda,
  feedSharePda,
  parseArgs,
  playerPda,
  printSignature,
  pushPda,
  roundPda,
  sol,
  starPda,
} from './lib';
import {
  LOCKED_ECONOMICS,
  PUSH_STEP,
  floorToStep,
  pushBoundsForStar,
} from '../../shared/chain/economics.js';

/**
 * Recover the error a failed send actually reported.
 *
 * web3.js renders `SendTransactionError` from an options bag, and when the
 * thrower omits `action` - which some Anchor versions do - the message becomes
 * `Unknown action 'undefined'` with the program's own message and logs gone.
 * Simulating the same instruction gets them back.
 */
async function unmask(err: any, builder: { simulate: () => Promise<unknown> }) {
  if (!/unknown action/i.test(String(err?.message ?? '')) || err?.transactionLogs) return err;
  try {
    await builder.simulate();
    return err;
  } catch (sim: any) {
    const logs = sim?.simulationResponse?.logs ?? sim?.logs;
    if (Array.isArray(logs)) console.error(logs.join('\n'));
    return sim;
  }
}

async function main() {
  const args = parseArgs();
  const { program, programId, connection, wallet, config, vault } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die('Config not found. Run `yarn initialize`.');

  const starId = args.star ? BigInt(args.star as string) : BigInt(c.currentStarId.toString());
  if (starId === 0n) die('No star exists yet.');

  const starAcc = await program.account.star.fetch(starPda(programId, starId)).catch(() => null);
  if (!starAcc) die(`Star #${starId} not found.`);
  const bounds = pushBoundsForStar(
    LOCKED_ECONOMICS,
    starAcc.lifecycle,
    starAcc.prizePool,
    starAcc.pendingLamports ?? 0,
    starAcc.totalMass
  );
  const min = new BN(bounds.min.toString());
  const max = new BN(bounds.max.toString());
  const amountSol = Number(args.amount ?? Number(min.toString()) / LAMPORTS_PER_SOL);
  const wanted = Math.round(amountSol * LAMPORTS_PER_SOL);
  let amount: BN;
  if (args.raw) {
    // Send the number as typed. The program owns the step rule; this is how
    // you watch it say so rather than taking the client's word for it.
    amount = new BN(wanted);
    console.log(`--raw: sending ${sol(wanted)} unsnapped and unclipped`);
  } else {
    const snapped = floorToStep(wanted);
    if (snapped !== wanted) {
      console.log(`snapping ${sol(wanted)} → ${sol(snapped)} (whole ${sol(PUSH_STEP)} steps only)`);
    }
    amount = new BN(Math.max(PUSH_STEP, snapped));
    if (amount.gt(max)) {
      console.log(`clipping ${sol(amount)} → ${sol(max)} (room left)`);
      amount = max;
    }
  }

  const count = Number(args.count ?? 1);
  const nursery = BigInt(starAcc.totalMass.toString()) < 1_000_000_000n;
  const star = starPda(programId, starId);
  const playerStats = playerPda(programId, wallet.publicKey);

  console.log(`player     ${wallet.publicKey.toBase58()}`);
  console.log(`balance    ${sol(await connection.getBalance(wallet.publicKey))}`);
  console.log(`star       #${starId} ${star.toBase58()}`);
  console.log(`amount     ${sol(amount)} x${count}${nursery ? '  (feed, no VRF)' : ''}`);
  if (!nursery) {
    console.log(
      'overhead   PendingPush rent 0.0014224 SOL, back on close, plus the round\'s' +
        ' 0.0017577 SOL\n           if you open it, back once every member resolves.' +
        ' Randomness costs the player nothing.'
    );
  }
  console.log();

  if (nursery) {
    for (let i = 0; i < count; i++) {
      const sig = await program.methods
        .feed(new BN(starId.toString()), amount)
        .accountsPartial({
          player: wallet.publicKey,
          config,
          vault,
          star,
          playerStats,
          starFeed: feedPda(programId, starId),
          feedShare: feedSharePda(programId, starId, wallet.publicKey),
          systemProgram: SystemProgram.programId,
        })
        .rpc();
      printSignature(`feed ${i + 1}/${count}`, sig);
      console.log();
    }
    return;
  }

  for (let i = 0; i < count; i++) {
    const clientSeed = crypto.randomBytes(32);
    const pendingPush = pushPda(programId, wallet.publicKey, clientSeed);

    for (let attempt = 1; ; attempt++) {
      // Re-read the star every attempt: `round` is pinned by seed to
      // `star.current_round`, so a seal landing between this read and execution
      // makes the address wrong and the program rejects it on the seeds
      // constraint. Retrying is safe only because nothing landed - checked
      // below, not assumed, since a double push would cost real stake.
      const cur = await program.account.star.fetch(star);
      const roundId = BigInt(cur.currentRound.toString());
      const round = roundPda(programId, starId, roundId);
      const builder = program.methods
        .requestPush(new BN(starId.toString()), amount, Array.from(clientSeed))
        .accountsPartial({
          player: wallet.publicKey,
          config,
          vault,
          star,
          round,
          playerStats,
          pendingPush,
          starFeed: feedPda(programId, starId),
          feedShare: feedSharePda(programId, starId, wallet.publicKey),
          systemProgram: SystemProgram.programId,
        });

      let sig: string;
      try {
        sig = await builder.rpc();
      } catch (err) {
        const landed = await program.account.pendingPush.fetchNullable(pendingPush);
        const moved = BigInt((await program.account.star.fetch(star)).currentRound.toString());
        if (landed || moved === roundId || attempt >= 3) throw await unmask(err, builder);
        console.log(`  round #${roundId} sealed as this was sending; joining #${moved}`);
        continue;
      }

      printSignature(`push ${i + 1}/${count}`, sig);
      console.log(`  push account ${pendingPush.toBase58()}`);
      console.log(`  round #${roundId}    ${round.toBase58()}`);
      console.log();
      break;
    }
  }

  console.log('The crank seals the round and buys its draw. Watch with `yarn show-pending`.');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
