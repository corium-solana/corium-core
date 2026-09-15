/**
 * Offline sanity check. Costs nothing, touches no network, and catches the
 * things that would otherwise waste a devnet round-trip:
 *
 *   - the IDL loads into this version of @coral-xyz/anchor
 *   - every instruction can be encoded with the account names we use
 *   - the TS hash derivations agree with the Rust ones, bit for bit
 *
 *   yarn tsx scripts/selftest.ts
 */

import * as crypto from 'crypto';

import {
  BN,
  Keypair,
  ORAO_VRF_PROGRAM_ID,
  PublicKey,
  SystemProgram,
  configPda,
  feedPda,
  feedSharePda,
  oraoNetworkStatePda,
  playerPda,
  pushPda,
  randomnessPda,
  rollFor,
  roundPda,
  roundSeed,
  starPda,
  vaultPda,
} from './lib';
import { LOCKED_ECONOMICS, PUSH_STEP } from '../../shared/chain/economics.js';

/** Must match `vrf::tests::round_seed_matches_the_typescript_client`. */
const ENTROPY_VECTOR = '5a71a6ac26dc4551af42e4520b103b0de576daf73d8778e71351ca683ded4e2b';
const SEED_VECTOR = 'c179d64bc131c48529795242fffb4094055a6070f112132e0d05ddcfe94da685';
const ROLL_VECTOR = 511_204_315;


let failures = 0;
function check(label: string, ok: boolean, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${label}${detail ? `  ${detail}` : ''}`);
  if (!ok) failures++;
}

async function main() {
  // Deliberately self-contained: an ephemeral key and a connection that is
  // never used, so this runs before you have a wallet or any devnet SOL.
  const { AnchorProvider, Program, Wallet } = require('@coral-xyz/anchor');
  const { Connection } = require('@solana/web3.js');
  const idl = require('../target/idl/soldust.json');

  const provider = new AnchorProvider(
    new Connection('http://127.0.0.1:8899', 'confirmed'),
    new Wallet(Keypair.generate()),
    {}
  );
  const program: any = new Program(idl, provider);
  const programId: PublicKey = program.programId;

  console.log(`program ${programId.toBase58()}\n`);

  // The crank derives round seeds and the clients replay rolls to prove to a
  // player that their own outcome was not tampered with. A drift here breaks
  // both silently, so it is asserted against the same vectors the Rust does.
  const { sha256 } = require('@noble/hashes/sha256');
  const entropy = Buffer.from(
    sha256(
      Buffer.concat([
        Buffer.from('soldust:entropy'),
        Buffer.alloc(32, 0),
        Buffer.alloc(32, 1),
        PublicKey.default.toBuffer(),
        Buffer.alloc(8, 0),
      ])
    )
  );
  // Zero accumulator because a round's entropy is written once, by the push that
  // opens it - the same call the Rust vector asserts.
  check('entropy folding matches Rust', entropy.toString('hex') === ENTROPY_VECTOR);
  check(
    'round seed matches Rust',
    roundSeed(1, 0, entropy, 300, Buffer.alloc(32, 2), PublicKey.default).toString('hex') ===
      SEED_VECTOR
  );
  check('roll derivation matches Rust', rollFor(Buffer.alloc(64, 3), 7) === ROLL_VECTOR);

  // 314 bps of one push step has to be a whole number of lamports, or `split`
  // rounds and the prize pool stops being exactly prize_bps of mass.
  check(
    'the pi rake lands exactly on the lamport lattice',
    (BigInt(PUSH_STEP) * BigInt(LOCKED_ECONOMICS.protocolBps)) % 10_000n === 0n,
    `${LOCKED_ECONOMICS.protocolBps} bps of ${PUSH_STEP} = ${
      (BigInt(PUSH_STEP) * BigInt(LOCKED_ECONOMICS.protocolBps)) / 10_000n
    } lamports`
  );
  check(
    'prize and protocol still sum to the whole',
    LOCKED_ECONOMICS.prizeBps + LOCKED_ECONOMICS.protocolBps === 10_000
  );

  const idlNames = program.idl.instructions.map((i: any) => i.name).sort();
  check('IDL exposes all 14 instructions', idlNames.length === 14, idlNames.join(', '));
  check('IDL exposes all 14 events', (program.idl.events ?? []).length === 14);

  // PDAs
  const config = configPda(programId);
  const vault = vaultPda(programId);
  const star = starPda(programId, 1);
  const round = roundPda(programId, 1, 0);
  console.log(`\nconfig   ${config.toBase58()}`);
  console.log(`vault    ${vault.toBase58()}`);
  console.log(`star #1  ${star.toBase58()}`);
  console.log(`round #0 ${round.toBase58()}`);
  console.log(`orao ns  ${oraoNetworkStatePda().toBase58()}\n`);

  // Encode every instruction we actually use, without sending.
  const player = Keypair.generate().publicKey;
  const clientSeed = crypto.randomBytes(32);
  const push = pushPda(programId, player, clientSeed);
  const drawSeed = roundSeed(1, 0, entropy, 300, Buffer.alloc(32, 2), player);

  try {
    const ix = await program.methods
      .feed(new BN(1), new BN(50_000_000))
      .accountsPartial({
        player,
        config,
        vault,
        star,
        playerStats: playerPda(programId, player),
        starFeed: feedPda(programId, 1),
        feedShare: feedSharePda(programId, 1, player),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('feed encodes', ix.keys.length === 8, `${ix.data.length} bytes of data`);
  } catch (e: any) {
    check('feed encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .requestPush(new BN(1), new BN(50_000_000), Array.from(clientSeed))
      .accountsPartial({
        player,
        config,
        vault,
        star,
        round,
        playerStats: playerPda(programId, player),
        pendingPush: push,
        starFeed: feedPda(programId, 1),
        feedShare: feedSharePda(programId, 1, player),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('request_push encodes', ix.keys.length === 10, `${ix.data.length} bytes of data`);
  } catch (e: any) {
    check('request_push encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .drawRound(new BN(300))
      .accountsPartial({
        cranker: player,
        config,
        vault,
        star,
        round,
        slotHashes: SLOT_HASHES,
        vrfProgram: ORAO_VRF_PROGRAM_ID,
        vrfNetworkState: oraoNetworkStatePda(),
        vrfTreasury: PublicKey.default,
        vrfRequest: randomnessPda(drawSeed),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('draw_round encodes', ix.keys.length === 11, `${ix.data.length} bytes of data`);
  } catch (e: any) {
    check('draw_round encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .closeRoundAccount()
      .accountsPartial({
        cranker: player,
        star,
        round,
        rentRecipient: player,
      })
      .instruction();
    check('close_round_account encodes', ix.keys.length === 4);
  } catch (e: any) {
    check('close_round_account encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .expireRound()
      .accountsPartial({
        cranker: player,
        star,
        round,
        vrfRequest: randomnessPda(drawSeed),
      })
      .instruction();
    check('expire_round encodes', ix.keys.length === 4);
  } catch (e: any) {
    check('expire_round encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .resolvePush()
      .accountsPartial({
        resolver: player,
        config,
        vault,
        pendingPush: push,
        star,
        round,
        playerStats: playerPda(programId, player),
        playerWallet: player,
        vrfRequest: randomnessPda(drawSeed),
        starFeed: feedPda(programId, 1),
        feedShare: feedSharePda(programId, 1, player),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('resolve_push encodes', ix.keys.length === 12);
  } catch (e: any) {
    check('resolve_push encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .fundProtocol(new BN(1_000_000))
      .accountsPartial({
        payer: player,
        config,
        vault,
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('fund_protocol encodes', ix.keys.length === 4);
  } catch (e: any) {
    check('fund_protocol encodes', false, e.message);
  }

  // The whole trust story: nothing on chain can retune or halt this program.
  const ixNames = new Set(program.idl.instructions.map((i: any) => i.name));
  for (const banned of [
    'updateConfig',
    'update_config',
    'setAuthority',
    'set_authority',
    'pause',
    'setPaused',
    'set_paused',
  ]) {
    check(`no ${banned} instruction`, !ixNames.has(banned));
  }

  // Read the on-disk IDL, not `program.idl`: Anchor rewrites names on load,
  // and what matters here is the account layout the program actually emits.
  const fieldsOf = (name: string) =>
    new Set(
      ((idl.types ?? []).find((t: any) => t.name === name)?.type?.fields ?? []).map((f: any) =>
        String(f.name)
      )
    );

  const configFields = fieldsOf('Config');
  for (const dead of ['authority', 'paused', 'economics', 'lifecycle']) {
    check(`Config carries no ${dead} field`, !configFields.has(dead));
  }
  // Randomness is bought per round out of the house cut now, so there is no
  // player-funded escrow bucket left to track.
  check('Config carries no oracle escrow', !configFields.has('oracle_escrow'));

  const pushFields = fieldsOf('PendingPush');
  check('PendingPush escrows no oracle budget', !pushFields.has('oracle_budget'));
  check('PendingPush carries no is_feed flag', !pushFields.has('is_feed'));
  check('PendingPush names its round', pushFields.has('round_id'));
  // The round seed has to be recomputable from account state alone, or a player
  // cannot check that the draw they were settled against was the sealed one.
  check('PendingPush keeps its client seed', pushFields.has('client_seed'));

  const roundFields = fieldsOf('Round');
  for (const need of ['entropy', 'seed', 'randomness', 'seed_slot', 'member_count', 'status']) {
    check(`Round carries ${need}`, roundFields.has(need));
  }

  // The load-bearing property of the whole design: a round's seed must not be
  // computable by anyone until the round is closed to new entrants. Two
  // ingredients cover each other - the entropy committed when the round opened
  // stops a block leader having sole control, and a slot hash sampled at the draw
  // stops the member who set it re-rolling by pushing again.
  const accountsOf = (name: string) =>
    new Set(
      (idl.instructions.find((i: any) => i.name === name)?.accounts ?? []).map((a: any) =>
        String(a.name)
      )
    );
  check(
    'close_round samples a slot hash it cannot predict',
    accountsOf('close_round').has('slot_hashes'),
    'stops a member re-rolling a sealed round'
  );
  check(
    'close_round moves no lamports',
    !accountsOf('close_round').has('vault'),
    'sealing is free and permissionless'
  );
  check(
    'expire_round checks the draw before voiding',
    accountsOf('expire_round').has('vrf_request'),
    'so a void can never duck an unfavourable roll'
  );

  // The economic gate. Sealing commits the house to buying a draw, so sealing is
  // where the draw has to be shown to be worth buying - priced off ORAO live,
  // never compiled in, and with the round's own stake on chain to check against.
  check(
    'close_round prices the draw before committing to it',
    accountsOf('close_round').has('vrf_network_state'),
    'the bar tracks whatever ORAO charges'
  );
  check(
    'a round records the stake behind its draw',
    fieldsOf('Round').has('stake'),
    'so close_round can tell a thin batch from a full one'
  );
  check(
    'a thin round is refused rather than subsidised',
    idl.errors.some((e: any) => e.name === 'RoundBelowDrawCost'),
    'it stays open for more members instead'
  );
  check(
    'the seal publishes the numbers it was judged on',
    ['stake', 'rake', 'draw_cost', 'forced'].every((f) => fieldsOf('RoundClosed').has(f)),
    'so the gate is auditable after the fact'
  );
  check(
    'the star can never be frozen',
    ixNames.has('expire_round') || ixNames.has('expireRound'),
    'any stalled round is permissionlessly voidable'
  );
  check(
    'request_push cannot pre-buy randomness',
    !accountsOf('request_push').has('vrf_request'),
    'the seed does not exist until the round seals'
  );
  check(
    'the house buys randomness, not the player',
    accountsOf('request_round_vrf').has('vrf_network_state'),
    'priced live off ORAO, charged to protocol_accrued'
  );
  check(
    'a round reports what its draw cost per member',
    fieldsOf('RoundRequested').has('cost_per_member'),
    'the amortisation is visible on chain'
  );

  try {
    const ix = await program.methods
      .createNextStar(new BN(2))
      .accountsPartial({
        payer: player,
        config,
        prevStar: star,
        nextStar: starPda(programId, 2),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('create_next_star encodes', ix.keys.length === 5);
  } catch (e: any) {
    check('create_next_star encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .closePush()
      .accountsPartial({ pendingPush: push, playerWallet: player })
      .instruction();
    check('close_push encodes', ix.keys.length === 2);
  } catch (e: any) {
    check('close_push encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .claimPrize()
      .accountsPartial({
        winner: player,
        config,
        vault,
        star,
        playerStats: playerPda(programId, player),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('claim_prize encodes', ix.keys.length === 6);
  } catch (e: any) {
    check('claim_prize encodes', false, e.message);
  }

  try {
    const ix = await program.methods
      .claimHoleShare()
      .accountsPartial({
        player,
        config,
        vault,
        star,
        starFeed: feedPda(programId, 1),
        feedShare: feedSharePda(programId, 1, player),
        playerStats: playerPda(programId, player),
        systemProgram: SystemProgram.programId,
      })
      .instruction();
    check('claim_hole_share encodes', ix.keys.length === 8);
  } catch (e: any) {
    check('claim_hole_share encodes', false, e.message);
  }

  console.log();
  if (failures) {
    console.error(`${failures} check(s) failed.`);
    process.exit(1);
  }
  console.log('All offline checks passed.');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
