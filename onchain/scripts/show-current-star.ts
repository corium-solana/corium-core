/**
 * Print a star. Defaults to the current one.
 *
 *   yarn show-current-star
 *   yarn show-current-star --star 3
 */

import { FEED_MASS, PUSH_STEP, novaPpb, roomAt } from '../../shared/chain/economics.js';
import {
  accountExplorer,
  context,
  die,
  enumName,
  parseArgs,
  ppb,
  sol,
  stageName,
  starPda,
  ts,
} from './lib';

async function main() {
  const args = parseArgs();
  const { program, programId, config } = context(args);

  const c = await program.account.config.fetch(config).catch(() => null);
  if (!c) die(`Config not found. Run \`yarn initialize\`.`);

  const starId = args.star ? BigInt(args.star as string) : BigInt(c.currentStarId.toString());
  if (starId === 0n) die('No star exists yet. Run `yarn initialize`.');

  const address = starPda(programId, starId);
  const s = await program.account.star.fetch(address).catch(() => null);
  if (!s) die(`Star #${starId} not found at ${address.toBase58()}`);

  const status = enumName(s.status);
  const mass = BigInt(s.totalMass.toString());

  // What the next push is up against. There is no table to read: a push's
  // chance is its own share of the mass it creates, so the only inputs are
  // the current mass and how much you send.
  const committed = mass + BigInt(s.pendingLamports?.toString() ?? 0);
  const room = BigInt(roomAt(Number(committed)));
  const step = BigInt(PUSH_STEP);

  console.log(`== Star #${s.starId} (${status.toUpperCase()}) ==`);
  console.log(`address            ${address.toBase58()}`);
  console.log(`  ${accountExplorer(address)}`);
  console.log(`visual seed        ${Buffer.from(s.seed).toString('hex')}`);
  console.log(`stage              ${s.stage} ${stageName(s.stage)}`);
  console.log(`mass               ${sol(s.totalMass)}`);
  console.log(`prize pool         ${sol(s.prizePool)}`);
  console.log(`push room          ${sol(room)} (whole ${sol(step)} steps)`);
  if (committed < BigInt(FEED_MASS)) {
    console.log(`nova chance        none - nursery, a push here is a hole ticket`);
  } else if (room > 0n) {
    console.log(`nova at min push   ${ppb(novaPpb(Number(step), Number(committed + step)))}`);
    console.log(`nova at max push   ${ppb(novaPpb(Number(room), Number(committed + room)))}`);
  }
  console.log(`born               ${ts(s.birthTs)} (slot ${s.birthSlot})`);
  console.log(`successful pushes  ${s.successfulPushes}`);
  console.log(`pending pushes     ${s.pendingPushes}`);
  console.log(`pending escrow     ${sol(s.pendingLamports ?? 0)}`);
  console.log(`cancelled pushes   ${s.cancelledPushes}`);

  if (status === 'dead') {
    console.log('\n-- death --');
    console.log(`died               ${ts(s.deathTs)} (slot ${s.deathSlot})`);
    console.log(`star killer        ${s.killer.toBase58()}`);
    console.log(`winning push       #${s.killerPushId} ${s.killerPush.toBase58()}`);
    console.log(`final prize        ${sol(s.finalPrize)}`);
    console.log(`prize claimed      ${s.prizeClaimed}`);
    console.log(`roll / threshold   ${ppb(s.deathRollPpb)} < ${ppb(s.deathThresholdPpb)}`);
    console.log(`vrf output         ${Buffer.from(s.deathRandomness).toString('hex')}`);
    console.log(`next star created  ${s.nextStarCreated}`);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
