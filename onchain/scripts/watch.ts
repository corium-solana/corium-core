/**
 * Tail the program's events. This is exactly the stream the Three.js frontend
 * should consume - no account polling required.
 *
 *   yarn watch
 */

import { EventParser } from '@coral-xyz/anchor';

import { context, parseArgs, sol } from './lib';

function summarise(name: string, data: any): string {
  switch (name) {
    case 'StarCreated':
      return `star #${data.starId} born, seed ${Buffer.from(data.seed).toString('hex').slice(0, 16)}...`;
    case 'PushRequested':
      return `push #${data.pushId} on star #${data.starId} by ${data.player.toBase58().slice(0, 8)} for ${sol(data.amount)} (pending)`;
    case 'PushResolved':
      return `push #${data.pushId} on star #${data.starId} ${
        data.survived ? 'SURVIVED' : 'LETHAL'
      } roll ${data.rollPpb} vs ${data.thresholdPpb}, mass ${sol(data.starMass)}, +${data.stardustAwarded} STARDUST`;
    case 'PushCancelled':
      return `push #${data.pushId} on dead star #${data.starId} refunded ${sol(data.amount)}`;
    case 'StageChanged':
      return `star #${data.starId} stage ${data.fromStage} -> ${data.toStage} at ${sol(data.starMass)}`;
    case 'StarDestroyed':
      return `STAR #${data.starId} DESTROYED by ${data.killer.toBase58()} - prize ${sol(data.finalPrize)}`;
    case 'StarCollapsed':
      return `STAR #${data.starId} COLLAPSED - prize ${sol(data.finalPrize)} recycled ${sol(data.echo)} early ${sol(data.earlyVolume)}`;
    case 'HoleShareClaimed':
      return `star #${data.starId} hole share ${sol(data.amount)} claimed by ${data.player.toBase58()}`;
    case 'PrizeClaimed':
      return `star #${data.starId} prize ${sol(data.amount)} claimed by ${data.winner.toBase58()}`;
    default:
      return JSON.stringify(data, (_k, v) => (v?.toBase58 ? v.toBase58() : v));
  }
}

async function main() {
  const args = parseArgs();
  const { program, connection, programId } = context(args);
  const parser = new EventParser(programId, program.coder);

  console.log(`Watching ${programId.toBase58()}. Ctrl-C to stop.\n`);

  connection.onLogs(
    programId,
    (logs) => {
      if (logs.err) return;
      for (const event of parser.parseLogs(logs.logs)) {
        const time = new Date().toISOString().slice(11, 19);
        console.log(`[${time}] ${event.name.padEnd(16)} ${summarise(event.name, event.data)}`);
      }
    },
    'confirmed'
  );

  await new Promise(() => {});
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
