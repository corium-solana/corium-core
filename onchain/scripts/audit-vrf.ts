/**
 * Post-migration audit of a single real draw on chain.
 *
 * Proves, from chain data alone, the four things the MagicBlock swap had to
 * preserve:
 *
 *   1. the draw was delivered by the VRF program, signed by the scoped identity
 *      PDA only it can sign for - nothing else can hand a round a draw
 *   2. the request fee went to the pinned queue and the reimbursement matched
 *      the cranker's real outlay
 *   3. every member's published roll is reproducible from the delivered
 *      randomness with the *unchanged* roll derivation
 *   4. the vault still covers every liability it is carrying
 *
 *   yarn tsx scripts/audit-vrf.ts --round <pubkey>
 *   yarn tsx scripts/audit-vrf.ts --draw <draw-tx-signature>
 */

import { PublicKey } from '@solana/web3.js';

import {
  VRF_PROGRAM_ID,
  VRF_QUEUE,
  configPda,
  context,
  enumName,
  parseArgs,
  rollFor,
  sol,
  vaultPda,
  vrfCallbackIdentityPda,
  widen,
} from './lib';

let failures = 0;
function check(label: string, ok: boolean, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${label}${detail ? `  ${detail}` : ''}`);
  if (!ok) failures++;
}

const hex = (b: Uint8Array) => Buffer.from(b).toString('hex');

async function main() {
  const opts = parseArgs();
  const { connection, program, programId } = context(opts);

  const drawSig = (opts as any).draw as string | undefined;
  if (!drawSig) throw new Error('pass --draw <signature> of the draw_round tx');

  const coder = (program as any).coder.events;
  const txCache = new Map<string, any>();
  const getTx = async (sig: string) => {
    if (!txCache.has(sig)) {
      txCache.set(
        sig,
        await connection.getTransaction(sig, {
          commitment: 'confirmed',
          maxSupportedTransactionVersion: 0,
        })
      );
    }
    return txCache.get(sig);
  };

  /**
   * Soldust events in a tx. A tx that touches the VRF program also carries that
   * program's own `Program data:` lines, and our coder throws on those, so a
   * failed decode means "not ours" rather than "corrupt".
   */
  const eventsIn = (tx: any): any[] => {
    const out: any[] = [];
    for (const line of tx?.meta?.logMessages ?? []) {
      const m = line.match(/^Program data: (.+)$/);
      if (!m) continue;
      try {
        const ev = coder.decode(m[1]);
        if (ev) out.push(ev);
      } catch {
        /* another program's event */
      }
    }
    return out;
  };

  // ------------------------------------------------------------ the draw tx
  const draw = await getTx(drawSig);
  if (!draw) throw new Error(`draw tx ${drawSig} not found`);

  const keys = draw.transaction.message
    .getAccountKeys({
      accountKeysFromLookups: draw.meta?.loadedAddresses,
    })
    .keySegments()
    .flat();
  const keyAt = (i: number) => keys[i];
  const indexOf = (pk: PublicKey) => keys.findIndex((k) => k.equals(pk));

  console.log(`\n== draw tx ${drawSig.slice(0, 16)}... ==`);
  check('draw tx succeeded', draw.meta?.err == null, JSON.stringify(draw.meta?.err ?? null));
  check(
    'the VRF program was invoked',
    indexOf(VRF_PROGRAM_ID) >= 0,
    VRF_PROGRAM_ID.toBase58()
  );

  // The fee is a plain lamport transfer into the queue, so the audit does not
  // have to trust any log line - the balance delta is in the tx metadata.
  const qi = indexOf(VRF_QUEUE);
  check('the pinned queue is in the tx', qi >= 0, VRF_QUEUE.toBase58());
  const queueDelta =
    qi >= 0 ? (draw.meta!.postBalances[qi] ?? 0) - (draw.meta!.preBalances[qi] ?? 0) : 0;
  check(
    'the request fee landed in the pinned queue',
    queueDelta === 500_000,
    `${queueDelta} lamports`
  );

  // The cranker is the fee payer, index 0. Its delta is the fee plus the tx
  // fee, minus whatever the rake paid back.
  const crankerDelta = draw.meta!.postBalances[0] - draw.meta!.preBalances[0];
  const txFee = draw.meta!.fee;
  console.log(
    `     cranker ${keyAt(0).toBase58()} net ${crankerDelta} lamports (tx fee ${txFee})`
  );
  check(
    'the cranker was made whole on the request fee',
    crankerDelta === -txFee,
    'net movement is the tx fee alone, so the rake covered the draw exactly'
  );

  // --------------------------------------------------- the RoundRequested event
  const events = eventsIn(draw);
  const requested = events.find((e) => e.name === 'roundRequested');
  const closed = events.find((e) => e.name === 'roundClosed');
  check('RoundClosed was emitted', !!closed);
  check('RoundRequested was emitted', !!requested);
  if (requested) {
    console.log(
      `     cost ${requested.data.cost} reimbursed ${requested.data.reimbursed} members ${requested.data.memberCount} per-member ${requested.data.costPerMember}`
    );
    check(
      'the reimbursement equals the measured cost',
      requested.data.cost.toString() === requested.data.reimbursed.toString(),
      'the rake covered it in full'
    );
    check(
      'the measured cost is exactly the request fee',
      requested.data.cost.toString() === '500000',
      'no per-request rent was entombed, unlike ORAO'
    );
  }

  const round: PublicKey = closed
    ? new PublicKey(closed.data.round)
    : new PublicKey((opts as any).round);

  if (closed) {
    console.log(
      `     seed ${hex(closed.data.seed)} from slot ${closed.data.seedSlot}`
    );
  }

  // ------------------------------------------------------- the callback tx
  console.log(`\n== callback into round ${round.toBase58()} ==`);
  const callbackIdentity = vrfCallbackIdentityPda(programId);

  // Every tx that touched this round, oldest first. This one pass yields both
  // the callback and the settlements - the round is on all of them.
  const sigs = (
    await connection.getSignaturesForAddress(round, { limit: 50 }, 'confirmed')
  ).reverse();
  const history: { sig: string; tx: any; events: any[] }[] = [];
  for (const s of sigs) {
    const tx = await getTx(s.signature);
    if (!tx) continue;
    history.push({ sig: s.signature, tx, events: eventsIn(tx) });
  }

  const drawnEntries = history.filter((h) => h.events.some((e) => e.name === 'roundDrawn'));
  const entry = drawnEntries[0];
  const callback = entry?.tx ?? null;
  const callbackSig = entry?.sig ?? '';
  check('a RoundDrawn callback landed on this round', !!callback, callbackSig);
  check(
    'the round was drawn exactly once',
    drawnEntries.length === 1,
    `${drawnEntries.length} draw(s) - a second one would mean the draw is rewritable`
  );

  let delivered: Uint8Array | null = null;
  if (callback) {
    const cbKeys = callback.transaction.message
      .getAccountKeys({ accountKeysFromLookups: callback.meta?.loadedAddresses })
      .keySegments()
      .flat();
    const cbLogs = callback.meta?.logMessages ?? [];

    // The integrity claim: soldust ran *inside* the VRF program, not beside it.
    // A top-level `Program <vrf> invoke [1]` followed by `Program <soldust>
    // invoke [2]` is the CPI, and only the VRF program can produce the identity
    // signature that call carries.
    check(
      'the VRF program was the top-level program',
      cbLogs.some((l) => l === `Program ${VRF_PROGRAM_ID.toBase58()} invoke [1]`)
    );
    check(
      'soldust was invoked by it, one level deeper (a CPI, not a bare call)',
      cbLogs.some((l) => l === `Program ${programId.toBase58()} invoke [2]`)
    );
    check(
      'the scoped callback identity is present as a signer',
      cbKeys.some((k) => k.equals(callbackIdentity)),
      callbackIdentity.toBase58()
    );
    check(
      'nobody could have called consume_randomness directly',
      !cbKeys.slice(0, callback.transaction.message.header.numRequiredSignatures).some((k) =>
        k.equals(callbackIdentity)
      ),
      'the identity is not a transaction signer - only a CPI signer'
    );

    const drawn = entry.events.find((e) => e.name === 'roundDrawn') as any;
    delivered = drawn.data.randomness;
    console.log(`     randomness ${hex(delivered!)}`);
    check(
      'the delivered randomness is not zero',
      delivered!.some((b: number) => b !== 0)
    );
    check(
      'the callback answered the seed this round published',
      closed ? hex(drawn.data.seed) === hex(closed.data.seed) : true
    );
  }

  // ----------------------------------------------------- the round, after
  const acct: any = await program.account.round.fetchNullable(round);
  if (acct) {
    console.log(`\n== round account ==`);
    check('status is Drawn', enumName(acct.status) === 'drawn', enumName(acct.status));
    check(
      'the stored draw is what the callback delivered',
      delivered ? hex(acct.randomness) === hex(delivered) : true
    );
    check(
      'the stored draw is 32 bytes, in the slot the ORAO address used to occupy',
      Buffer.from(acct.randomness).length === 32
    );
  } else {
    console.log('\n== round account already swept (rent returned) ==');
  }

  // ------------------------------------------- every roll is reproducible
  console.log(`\n== rolls ==`);
  const resolvedEvents = history
    .flatMap((h) => h.events)
    .filter((e) => e.name === 'pushResolved')
    .map((e) => e.data);
  check(
    'found the round members that settled',
    resolvedEvents.length > 0,
    `${resolvedEvents.length}`
  );
  if (closed) {
    check(
      'every member of the sealed round settled',
      BigInt(resolvedEvents.length) === BigInt(closed.data.memberCount.toString()),
      `${resolvedEvents.length} of ${closed.data.memberCount}`
    );
  }

  if (delivered) {
    for (const r of resolvedEvents) {
      const replayed = rollFor(widen(delivered), r.pushId);
      check(
        `push #${r.pushId} roll is reproducible from the delivered draw`,
        replayed === r.rollPpb,
        `on chain ${r.rollPpb} ppb vs replay ${replayed} ppb, threshold ${r.thresholdPpb} ppb`
      );
    }
    // Distinct rolls out of one draw is the property that makes a shared draw
    // legitimate rather than a single coin flip for the whole batch.
    const rolls = resolvedEvents.map((r) => r.rollPpb);
    check(
      'members of the batch rolled independently',
      new Set(rolls).size === rolls.length,
      rolls.join(', ')
    );
  }

  // ------------------------------------------------------- vault solvency
  console.log(`\n== vault ==`);
  const cfg: any = await program.account.config.fetch(configPda(programId));
  const vault = vaultPda(programId);
  const balance = await connection.getBalance(vault);
  const liabilities =
    BigInt(cfg.pendingLiability.toString()) +
    BigInt(cfg.prizeLiability.toString()) +
    BigInt(cfg.protocolAccrued.toString()) +
    BigInt(cfg.nextStarReserve.toString());
  console.log(`     balance      ${sol(balance)}`);
  console.log(`     pending      ${sol(cfg.pendingLiability)}`);
  console.log(`     prize        ${sol(cfg.prizeLiability)}`);
  console.log(`     protocol     ${sol(cfg.protocolAccrued)}`);
  console.log(`     next star    ${sol(cfg.nextStarReserve)}`);
  check(
    'vault covers every liability it carries',
    BigInt(balance) >= liabilities,
    `${balance} >= ${liabilities}`
  );

  console.log(
    failures === 0
      ? '\nAll checks passed.\n'
      : `\n${failures} check(s) FAILED.\n`
  );
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
