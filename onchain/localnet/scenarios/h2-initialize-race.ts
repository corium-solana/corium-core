/**
 * H-2 (fixed) - only the program's deployer can name the treasury.
 *
 * The bug: `initialize` had no signer gate. It takes `treasury` as a plain
 * argument and stores it on `Config`, and there is no admin and no setter - which
 * is a deliberate and good property, but it meant the value was frozen by the
 * *first* transaction to land after deploy, whoever sent it. On mainnet that is a
 * public, watchable window between `solana program deploy` and the operator's own
 * initialize. Losing that race cost all protocol revenue with no recourse short of
 * redeploying at a fresh program id.
 *
 * The fix asks the loader who deployed: the signer must be the program's current
 * BPF upgrade authority. Nothing is *stored* - the check reads `ProgramData`, so
 * it cannot go stale and needs no configuration. The one ordering consequence is
 * that initialize has to happen before upgrade authority is dropped to `None`,
 * which the last act below demonstrates the hard way.
 */

import { Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram } from '@solana/web3.js';

import { BN } from '@coral-xyz/anchor';

import {
  BPF_LOADER_UPGRADEABLE,
  act,
  assert,
  conclude,
  connect,
  createFirstStar,
  feed,
  head,
  initialize,
  mustFail,
  newPlayer,
  say,
  sol,
  vrfInit,
  FEED_MASS,
} from '../lib';

async function main() {
  head('H-2: initialize is gated on the program\'s upgrade authority');
  const w = await connect();

  // A keypair rather than a bare address because the withdrawal at the end signs
  // as the treasury: this star's whole rake is inside `DRAW_FLOAT_FLOOR`, and the
  // treasury is the one caller allowed to reach into that. See `m4-float-floor`.
  const operator = Keypair.generate();
  const operatorTreasury = operator.publicKey;
  const attackerTreasury = Keypair.generate().publicKey;
  await vrfInit(w);

  act('confirm the harness really is standing where mainnet will');
  const pd = await w.connection.getAccountInfo(w.programData);
  assert(pd !== null, 'the program has a real ProgramData account');
  assert(pd!.owner.equals(BPF_LOADER_UPGRADEABLE), 'owned by the BPF upgradeable loader');
  const authority = pd!.data[12] === 1 ? new PublicKey(pd!.data.subarray(13, 45)) : null;
  say(`upgrade authority: ${authority?.toBase58() ?? 'none'}`);
  assert(
    authority !== null && authority.equals(w.deployer.publicKey),
    'and its upgrade authority is the deployer keypair'
  );

  act('a stranger tries to front-run the operator\'s initialize');
  const attacker = await newPlayer(w, 10 * LAMPORTS_PER_SOL);
  const code = await mustFail('attacker initialize', () =>
    initialize(w, attackerTreasury, attacker)
  );
  assert(code === 'NotProgramAuthority', 'refused with NotProgramAuthority');

  act('and so does the harness\'s own well-funded payer, which is nobody special');
  const code2 = await mustFail('random payer initialize', () =>
    initialize(w, attackerTreasury, w.payer)
  );
  assert(code2 === 'NotProgramAuthority', 'refused too - funding is not standing');
  let exists = true;
  try {
    await w.program.account.config.fetch(w.config);
  } catch {
    exists = false;
  }
  assert(!exists, 'no Config exists yet, so nothing has been decided');

  act('the deployer initializes, and gets the treasury it asked for');
  await initialize(w, operatorTreasury, w.deployer);
  const cfg: any = await w.program.account.config.fetch(w.config);
  say(`config.treasury = ${cfg.treasury.toString()}`);
  assert(
    cfg.treasury.toString() === operatorTreasury.toString(),
    'the treasury is the operator\'s'
  );

  act('and it stays that way - the gate protects a value nothing can revise');
  const again = await mustFail('deployer initialize a second time', () =>
    initialize(w, attackerTreasury, w.deployer)
  );
  say(`(${again} - the config PDA is already in use)`);
  const setters = (w.program.idl.instructions as any[])
    .map((i) => i.name)
    .filter((n) => /treasury|admin|authority|set_/.test(n));
  assert(
    setters.length === 0,
    `no instruction can change it (searched all ${w.program.idl.instructions.length})`
  );

  act('revenue accrues and is payable only to that address');
  await createFirstStar(w);
  const player = await newPlayer(w);
  await feed(w, 1, FEED_MASS, player);
  const after: any = await w.program.account.config.fetch(w.config);
  say(`protocol_accrued after 1 SOL of volume: ${sol(after.protocolAccrued)} (314 bps)`);

  const before = await w.connection.getBalance(operatorTreasury);
  await w.program.methods
    .withdrawProtocolFees(new BN(Number(after.protocolAccrued)))
    .accountsPartial({
      crank: operatorTreasury,
      config: w.config,
      vault: w.vault,
      treasury: operatorTreasury,
      systemProgram: SystemProgram.programId,
    })
    .signers([operator])
    .rpc();
  const paid = (await w.connection.getBalance(operatorTreasury)) - before;
  say(`withdraw_protocol_fees paid ${sol(paid)} to the operator`);
  assert(paid > 0, 'revenue reaches the operator');

  const wrong = await mustFail('withdraw to the attacker instead', () =>
    w.program.methods
      .withdrawProtocolFees(new BN(1))
      .accountsPartial({
        crank: w.payer.publicKey,
        config: w.config,
        vault: w.vault,
        treasury: attackerTreasury,
        systemProgram: SystemProgram.programId,
      })
      .rpc()
  );
  assert(
    wrong === 'ConstraintHasOne' || /Treasury|HasOne/i.test(wrong),
    `and nowhere else (${wrong})`
  );

  act('the deployment order this implies');
  say('deploy -> initialize (signed by the upgrade authority) -> only then set');
  say('  upgrade authority to none. Reversing those two makes the game unstartable,');
  say('  because there would be no authority left for the gate to match.');

  process.exit(conclude('H-2'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
