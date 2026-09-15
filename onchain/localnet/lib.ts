/**
 * Harness for the local-validator reproductions.
 *
 * Deliberately self-contained: it talks to the program through the IDL and
 * nothing else, so a scenario passing or failing says something about the
 * program rather than about `shared/chain`. The mock ORAO instructions are
 * hand-encoded for the same reason.
 */

import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';

import { AnchorProvider, BN, Program, Wallet } from '@coral-xyz/anchor';
import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  SYSVAR_SLOT_HASHES_PUBKEY,
  Transaction,
  TransactionInstruction,
} from '@solana/web3.js';

export const RPC = process.env.LOCALNET_RPC ?? 'http://127.0.0.1:8899';

export const ORAO_ID = new PublicKey('VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y');
export const BPF_LOADER_UPGRADEABLE = new PublicKey(
  'BPFLoaderUpgradeab1e11111111111111111111111'
);
/** ORAO's real mainnet request fee, so the economics gate behaves as it will live. */
export const REQUEST_FEE = 500_000;

export const PUSH_STEP = LAMPORTS_PER_SOL / 100;
export const FEED_MASS = LAMPORTS_PER_SOL;
export const ROUND_WINDOW_SLOTS = 75;
export const ROUND_EXPIRY_SLOTS = 750;
export const PROTOCOL_BPS = 314;
/** What a permissionless `withdraw_protocol_fees` has to leave for the oracle. */
export const DRAW_FLOAT_FLOOR = LAMPORTS_PER_SOL / 20;

// The stall windows as a `short-stalls` build sees them: seconds, not the
// shipped week and day. Only `c3-stalled-star` uses them, and only against that
// build - see `build-short-stalls.sh`.
export const STALL_SECS = 25;
export const NURSERY_STALL_SECS = 10;

export const u64 = (n: number | bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(BigInt(n));
  return b;
};

export const sol = (lamports: number | bigint | BN) => {
  const n = typeof lamports === 'object' ? BigInt(lamports.toString()) : BigInt(lamports);
  const neg = n < 0n;
  const abs = neg ? -n : n;
  return `${neg ? '-' : ''}${(Number(abs) / LAMPORTS_PER_SOL).toFixed(6)} SOL`;
};

// ------------------------------------------------------------------ logging

let step = 0;
export const say = (msg: string) => console.log(`  ${msg}`);
export const act = (msg: string) => console.log(`\n[${++step}] ${msg}`);
export const head = (msg: string) => console.log(`\n${'='.repeat(74)}\n${msg}\n${'='.repeat(74)}`);
export const ok = (msg: string) => console.log(`  PASS  ${msg}`);
export const bad = (msg: string) => console.log(`  FAIL  ${msg}`);

const failures: string[] = [];
export function assert(cond: boolean, msg: string) {
  if (cond) ok(msg);
  else {
    bad(msg);
    failures.push(msg);
  }
}
export function conclude(name: string) {
  console.log();
  if (failures.length === 0) {
    head(`${name}: every assertion held`);
    return 0;
  }
  head(`${name}: ${failures.length} assertion(s) failed\n  - ${failures.join('\n  - ')}`);
  return 1;
}

// ------------------------------------------------------------------- errors

/** Anchor error code name, however the RPC chose to wrap it. */
export function errCode(e: any): string {
  const direct = e?.error?.errorCode?.code;
  if (direct) return direct;
  const logs: string[] = e?.logs ?? e?.transactionLogs ?? [];
  for (const line of logs) {
    const m = line.match(/Error Code: (\w+)/);
    if (m) return m[1];
  }
  const m = String(e?.message ?? e).match(/Error Code: (\w+)/);
  return m ? m[1] : String(e?.message ?? e).slice(0, 160);
}

/** Run something that must fail, and report which error came back. */
export async function mustFail(what: string, fn: () => Promise<any>): Promise<string> {
  try {
    await fn();
    bad(`${what} - expected a failure, but it succeeded`);
    failures.push(`${what} unexpectedly succeeded`);
    return '<succeeded>';
  } catch (e) {
    const code = errCode(e);
    say(`${what} -> rejected with ${code}`);
    return code;
  }
}

// ---------------------------------------------------------------- the world

export type World = Awaited<ReturnType<typeof connect>>;

export async function connect() {
  const connection = new Connection(RPC, 'confirmed');
  const payer = Keypair.generate();
  const provider = new AnchorProvider(connection, new Wallet(payer), {
    commitment: 'confirmed',
    preflightCommitment: 'confirmed',
  });
  const idl = JSON.parse(
    fs.readFileSync(path.resolve(__dirname, '../target/idl/soldust.json'), 'utf8')
  );
  const program = new Program(idl as any, provider) as any;
  const programId = new PublicKey(idl.address);

  const pda = (seeds: (Buffer | Uint8Array)[]) => PublicKey.findProgramAddressSync(seeds, programId)[0];

  // The keypair `validator.sh` handed the program's upgrade authority to.
  // `initialize` will only accept this signer, which is the whole of H-2's fix.
  const deployer = Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(path.resolve(__dirname, 'deployer.json'), 'utf8')))
  );

  const w = {
    connection,
    provider,
    program,
    programId,
    payer,
    deployer,
    programData: PublicKey.findProgramAddressSync(
      [programId.toBuffer()],
      BPF_LOADER_UPGRADEABLE
    )[0],
    config: pda([Buffer.from('config')]),
    vault: pda([Buffer.from('vault')]),
    star: (id: number) => pda([Buffer.from('star'), u64(id)]),
    round: (starId: number, roundId: number) =>
      pda([Buffer.from('round'), u64(starId), u64(roundId)]),
    push: (player: PublicKey, seed: Buffer) =>
      pda([Buffer.from('push'), player.toBuffer(), seed]),
    playerStats: (player: PublicKey) => pda([Buffer.from('player'), player.toBuffer()]),
    starFeed: (starId: number) => pda([Buffer.from('feed'), u64(starId)]),
    feedShare: (starId: number, player: PublicKey) =>
      pda([Buffer.from('feed-share'), u64(starId), player.toBuffer()]),
    oraoNetwork: PublicKey.findProgramAddressSync(
      [Buffer.from('orao-vrf-network-configuration')],
      ORAO_ID
    )[0],
    oraoRequest: (seed: Buffer | Uint8Array) =>
      PublicKey.findProgramAddressSync(
        [Buffer.from('orao-vrf-randomness-request'), Buffer.from(seed)],
        ORAO_ID
      )[0],
  };

  await fund(w, payer.publicKey, 500 * LAMPORTS_PER_SOL);
  await fund(w, deployer.publicKey, 100 * LAMPORTS_PER_SOL);
  return w;
}

export async function fund(w: World, who: PublicKey, lamports: number) {
  const sig = await w.connection.requestAirdrop(who, lamports);
  await w.connection.confirmTransaction(sig, 'confirmed');
}

export async function newPlayer(w: World, lamports = 5 * LAMPORTS_PER_SOL) {
  const kp = Keypair.generate();
  await fund(w, kp.publicKey, lamports);
  return kp;
}

export const slot = (w: World) => w.connection.getSlot('confirmed');

export async function waitUntilSlot(w: World, target: number, label: string) {
  let now = await slot(w);
  if (now >= target) return now;
  say(`waiting for slot ${target} (${target - now} slots) - ${label}`);
  while (now < target) {
    await new Promise((r) => setTimeout(r, 400));
    now = await slot(w);
  }
  return now;
}

async function send(w: World, ixs: TransactionInstruction[], signers: Keypair[]) {
  const tx = new Transaction().add(...ixs);
  return w.provider.sendAndConfirm(tx, signers, { commitment: 'confirmed' });
}

// -------------------------------------------------------- mock ORAO clients

const disc = (name: string) =>
  createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);

/**
 * ORAO's genesis network configuration. Sets the request fee soldust prices
 * draws against.
 */
export async function oraoInit(w: World, treasury: PublicKey, fee = REQUEST_FEE) {
  // ORAO's live treasury is a long-funded account. An empty one here would make
  // every request fail the runtime's rent-exemption check on the fee transfer,
  // which is an artefact of the harness rather than anything about soldust.
  await fund(w, treasury, LAMPORTS_PER_SOL);
  const data = Buffer.concat([disc('mock_init_network'), u64(fee), treasury.toBuffer()]);
  const ix = new TransactionInstruction({
    programId: ORAO_ID,
    keys: [
      { pubkey: w.payer.publicKey, isSigner: true, isWritable: true },
      { pubkey: w.oraoNetwork, isSigner: false, isWritable: true },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
    ],
    data,
  });
  return send(w, [ix], [w.payer]);
}

/** What ORAO's oracles do off-chain: land the draw. */
export async function oraoFulfill(w: World, request: PublicKey, fill?: Buffer) {
  const randomness = fill ?? Buffer.alloc(64, 0x11);
  const ix = new TransactionInstruction({
    programId: ORAO_ID,
    keys: [
      { pubkey: w.payer.publicKey, isSigner: true, isWritable: false },
      { pubkey: request, isSigner: false, isWritable: true },
    ],
    data: Buffer.concat([disc('mock_fulfill'), randomness]),
  });
  return send(w, [ix], [w.payer]);
}

/** Simulate ORAO moving the layout soldust hardcodes. 0=discriminator, 1=tag, 2=seed. */
export async function oraoCorrupt(w: World, request: PublicKey, mode: number) {
  const ix = new TransactionInstruction({
    programId: ORAO_ID,
    keys: [
      { pubkey: w.payer.publicKey, isSigner: true, isWritable: false },
      { pubkey: request, isSigner: false, isWritable: true },
    ],
    data: Buffer.concat([disc('mock_corrupt'), Buffer.from([mode])]),
  });
  return send(w, [ix], [w.payer]);
}

/**
 * Call ORAO's `request_v2` directly, as any stranger can. Used to occupy the
 * address a sealed round has already committed to.
 */
export async function oraoRequestDirect(
  w: World,
  from: Keypair,
  seed: Buffer,
  treasury: PublicKey
) {
  const ix = new TransactionInstruction({
    programId: ORAO_ID,
    keys: [
      { pubkey: from.publicKey, isSigner: true, isWritable: true },
      { pubkey: w.oraoNetwork, isSigner: false, isWritable: true },
      { pubkey: treasury, isSigner: false, isWritable: true },
      { pubkey: w.oraoRequest(seed), isSigner: false, isWritable: true },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
    ],
    data: Buffer.concat([disc('request_v2'), seed]),
  });
  const tx = new Transaction().add(ix);
  tx.feePayer = from.publicKey;
  const { blockhash } = await w.connection.getLatestBlockhash();
  tx.recentBlockhash = blockhash;
  tx.sign(from);
  const sig = await w.connection.sendRawTransaction(tx.serialize());
  await w.connection.confirmTransaction(sig, 'confirmed');
  return sig;
}

// ------------------------------------------------------------ soldust calls

/**
 * Defaults to the program's upgrade authority, which is the only signer the
 * program accepts. Pass someone else to prove that it refuses them.
 */
export async function initialize(w: World, treasury: PublicKey, signer: Keypair = w.deployer) {
  return w.program.methods
    .initialize(Array.from(Buffer.alloc(32, 7)), treasury)
    .accountsPartial({
      payer: signer.publicKey,
      config: w.config,
      vault: w.vault,
      programData: w.programData,
      systemProgram: SystemProgram.programId,
    })
    .signers([signer])
    .rpc();
}

export async function fundProtocol(w: World, lamports: number) {
  return w.program.methods
    .fundProtocol(new BN(lamports))
    .accountsPartial({
      payer: w.payer.publicKey,
      config: w.config,
      vault: w.vault,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}

/**
 * Collect protocol revenue. Permissionless, so a stranger (`w.payer`) cranks it
 * by default and gets nothing for it; pass the treasury's own keypair as `as` to
 * exercise the one caller allowed below `DRAW_FLOAT_FLOOR`.
 */
export async function withdrawFees(
  w: World,
  treasury: PublicKey,
  lamports: number,
  opts: { as?: Keypair } = {}
) {
  const crank = opts.as ?? w.payer;
  return w.program.methods
    .withdrawProtocolFees(new BN(lamports))
    .accountsPartial({
      crank: crank.publicKey,
      config: w.config,
      vault: w.vault,
      treasury,
      systemProgram: SystemProgram.programId,
    })
    .signers(crank === w.payer ? [] : [crank])
    .rpc();
}

export async function createFirstStar(w: World) {
  return w.program.methods
    .createFirstStar()
    .accountsPartial({
      payer: w.payer.publicKey,
      config: w.config,
      star: w.star(1),
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}

export async function feed(w: World, starId: number, lamports: number, player: Keypair = w.payer) {
  return w.program.methods
    .feed(new BN(starId), new BN(lamports))
    .accountsPartial({
      player: player.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(starId),
      playerStats: w.playerStats(player.publicKey),
      starFeed: w.starFeed(starId),
      feedShare: w.feedShare(starId, player.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .signers(player === w.payer ? [] : [player])
    .rpc();
}

export type Push = {
  player: Keypair;
  seed: Buffer;
  pda: PublicKey;
  pushId: number;
  roundId: number;
  amount: number;
};

export async function requestPush(
  w: World,
  starId: number,
  lamports: number,
  player: Keypair
): Promise<Push> {
  const seed = Buffer.from(createHash('sha256').update(Keypair.generate().publicKey.toBuffer()).digest());
  const star: any = await w.program.account.star.fetch(w.star(starId));
  const roundId = Number(star.currentRound);
  const pda = w.push(player.publicKey, seed);

  await w.program.methods
    .requestPush(new BN(starId), new BN(lamports), Array.from(seed))
    .accountsPartial({
      player: player.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(starId),
      round: w.round(starId, roundId),
      playerStats: w.playerStats(player.publicKey),
      pendingPush: pda,
      starFeed: w.starFeed(starId),
      feedShare: w.feedShare(starId, player.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .signers([player])
    .rpc();

  const push: any = await w.program.account.pendingPush.fetch(pda);
  return {
    player,
    seed,
    pda,
    pushId: Number(push.pushId),
    roundId: Number(push.roundId),
    amount: Number(push.amount),
  };
}

/** Mirrors `vrf::round_seed`. Needed before sending, to name the ORAO account. */
export function roundSeed(
  starId: number,
  roundId: number,
  entropy: Buffer | Uint8Array,
  seedSlot: number | bigint,
  slotHash: Buffer | Uint8Array,
  cranker: PublicKey
): Buffer {
  return createHash('sha256')
    .update(
      Buffer.concat([
        Buffer.from('soldust:round'),
        u64(starId),
        u64(roundId),
        Buffer.from(entropy),
        u64(seedSlot),
        Buffer.from(slotHash),
        cranker.toBuffer(),
      ])
    )
    .digest();
}

/** The newest `(slot, hash)` in SlotHashes, parsed as `vrf::slot_hash_at` does. */
export async function recentSlotHash(w: World) {
  const info = await w.connection.getAccountInfo(SYSVAR_SLOT_HASHES_PUBKEY);
  if (!info || info.data.length < 48) throw new Error('SlotHashes unreadable');
  return { slot: info.data.readBigUInt64LE(8), hash: info.data.subarray(16, 48) };
}

/**
 * Seal a round and buy its draw, in one transaction.
 *
 * The caller has to derive the seed itself now, because the ORAO account address
 * follows from it and Solana needs every address up front. `seedSlot` lets a
 * scenario pin the slot to reproduce a specific address; otherwise it takes the
 * newest one, which is what the real crank does.
 */
export async function drawRound(
  w: World,
  starId: number,
  roundId: number,
  treasury: PublicKey,
  opts: { cranker?: Keypair; seedSlot?: bigint; slotHash?: Buffer } = {}
) {
  const cranker = opts.cranker ?? w.payer;
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  const recent = await recentSlotHash(w);
  const seedSlot = opts.seedSlot ?? recent.slot;
  const slotHash = opts.slotHash ?? recent.hash;
  const seed = roundSeed(starId, roundId, round.entropy, seedSlot, slotHash, cranker.publicKey);
  const request = w.oraoRequest(seed);

  await w.program.methods
    .drawRound(new BN(seedSlot.toString()))
    .accountsPartial({
      cranker: cranker.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(starId),
      round: w.round(starId, roundId),
      slotHashes: SYSVAR_SLOT_HASHES_PUBKEY,
      vrfProgram: ORAO_ID,
      vrfNetworkState: w.oraoNetwork,
      vrfTreasury: treasury,
      vrfRequest: request,
      systemProgram: SystemProgram.programId,
    })
    .signers(cranker === w.payer ? [] : [cranker])
    .rpc();

  return { seed, request, seedSlot };
}

export async function closeRoundAccount(w: World, starId: number, roundId: number) {
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  return w.program.methods
    .closeRoundAccount()
    .accountsPartial({
      cranker: w.payer.publicKey,
      star: w.star(starId),
      round: w.round(starId, roundId),
      rentRecipient: new PublicKey(round.openedBy),
    })
    .rpc();
}

export async function expireRound(w: World, starId: number, roundId: number) {
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  const randomness = new PublicKey(round.randomness);
  return w.program.methods
    .expireRound()
    .accountsPartial({
      cranker: w.payer.publicKey,
      star: w.star(starId),
      round: w.round(starId, roundId),
      // Default when the round never got as far as being requested; the program
      // only reads it in the `Requested` branch.
      vrfRequest: randomness.equals(PublicKey.default) ? SystemProgram.programId : randomness,
    })
    .rpc();
}

export async function resolvePush(w: World, p: Push) {
  const push: any = await w.program.account.pendingPush.fetch(p.pda);
  const starId = Number(push.starId);
  const roundId = Number(push.roundId);
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  const randomness = new PublicKey(round.randomness);
  return w.program.methods
    .resolvePush()
    .accountsPartial({
      resolver: w.payer.publicKey,
      config: w.config,
      vault: w.vault,
      pendingPush: p.pda,
      star: w.star(starId),
      round: w.round(starId, roundId),
      playerStats: w.playerStats(new PublicKey(push.player)),
      playerWallet: new PublicKey(push.player),
      vrfRequest: randomness.equals(PublicKey.default) ? SystemProgram.programId : randomness,
      starFeed: w.starFeed(starId),
      feedShare: w.feedShare(starId, new PublicKey(push.player)),
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}

// ------------------------------------------------------------- observations

export const statusName = (s: any): string =>
  typeof s === 'string' ? s : Object.keys(s ?? {})[0] ?? '?';

export async function showStar(w: World, starId: number, label = 'star') {
  const s: any = await w.program.account.star.fetch(w.star(starId));
  say(
    `${label}: status=${statusName(s.status)} mass=${sol(s.totalMass)} ` +
      `settle_cursor=${s.settleCursor} push_counter=${s.pushCounter} ` +
      `pending=${s.pendingPushes} cancelled=${s.cancelledPushes} current_round=${s.currentRound}`
  );
  return s;
}

export async function showRound(w: World, starId: number, roundId: number) {
  const r: any = await w.program.account.round.fetch(w.round(starId, roundId));
  say(
    `round #${roundId}: status=${statusName(r.status)} members=${r.memberCount} ` +
      `stake=${sol(r.stake)} first_push=${r.firstPushId}`
  );
  return r;
}

export async function pushStatus(w: World, p: Push) {
  const push: any = await w.program.account.pendingPush.fetch(p.pda);
  return statusName(push.status);
}

/**
 * The economic bar `draw_round` enforces: a round only seals once its own rake
 * covers what ORAO charges. Mirrors `Round::rake` and `vrf::draw_cost_estimate`.
 */
export async function drawCost(w: World) {
  const rentFulfilled = await w.connection.getMinimumBalanceForRentExemption(137);
  return REQUEST_FEE + rentFulfilled;
}

export async function stakeNeededForDraw(w: World) {
  const cost = await drawCost(w);
  const raw = Math.ceil((cost * 10_000) / PROTOCOL_BPS);
  return Math.ceil(raw / PUSH_STEP) * PUSH_STEP;
}

// ------------------------------------------------------- forcing an outcome

/** Mirrors `vrf::roll_for`: sha256("soldust:roll" ‖ draw ‖ push_id_le)[..16] mod 1e9. */
export function rollFor(randomness: Buffer, pushId: number): number {
  const h = createHash('sha256')
    .update(Buffer.concat([Buffer.from('soldust:roll'), randomness, u64(pushId)]))
    .digest();
  let v = 0n;
  for (let i = 15; i >= 0; i--) v = (v << 8n) | BigInt(h[i]);
  return Number(v % 1_000_000_000n);
}

/** Mirrors `game_config::nova_ppb`. */
export const novaPpb = (accepted: number, massAfter: number) =>
  Number((BigInt(accepted) * 1_000_000_000n) / BigInt(massAfter));

/**
 * Search for a draw that makes one specific member's roll lethal.
 *
 * Being able to do this is not a finding - it needs control of the oracle's
 * output, which is what a mock is for. It is how a supernova gets tested
 * deterministically instead of by playing until one happens.
 */
export function grindRoll(pushId: number, thresholdPpb: number, lethal = true): Buffer {
  for (let i = 0; i < 5_000_000; i++) {
    const r = Buffer.alloc(64);
    r.writeUInt32LE(i, 0);
    r.writeUInt32LE(i ^ 0x5a5a5a5a, 60);
    if (rollFor(r, pushId) < thresholdPpb === lethal) return r;
  }
  throw new Error(`no draw found for push ${pushId} at threshold ${thresholdPpb}`);
}

/**
 * A draw that is lethal for nobody in a batch.
 *
 * The mirror of `grindRoll`, for scenarios whose point is that a round *settled*
 * rather than which way it settled. Members of one round share a draw, so their
 * outcomes are fixed by the fill rather than chosen - and a test about liveness
 * should not be hostage to whether the first roll happened to kill the star.
 */
export function grindSurvival(members: { pushId: number; thresholdPpb: number }[]): Buffer {
  for (let i = 0; i < 5_000_000; i++) {
    const r = Buffer.alloc(64);
    r.writeUInt32LE(i, 0);
    r.writeUInt32LE(i ^ 0x5a5a5a5a, 60);
    if (members.every((m) => rollFor(r, m.pushId) >= m.thresholdPpb)) return r;
  }
  throw new Error('no draw found where every member survives');
}

/**
 * Bring a fresh world to the point where last-hit pushes are legal: config,
 * a funded protocol float, star #1, and a full nursery.
 */
export async function bootstrap(w: World, treasury: PublicKey) {
  act('bootstrap: initialize, fund the float, birth star #1, fill the nursery');
  await oraoInit(w, treasury);
  say(`mock ORAO network state at ${w.oraoNetwork.toBase58()} (fee ${REQUEST_FEE})`);
  await initialize(w, treasury);
  await fundProtocol(w, 2 * LAMPORTS_PER_SOL);
  await createFirstStar(w);
  await feed(w, 1, FEED_MASS);
  const s = await showStar(w, 1, 'after nursery');
  return s;
}

/** Wait out a round's window, seal-and-draw it, and optionally land the draw. */
export async function sealAndDraw(
  w: World,
  starId: number,
  roundId: number,
  treasury: PublicKey,
  opts: { fulfill?: boolean } = {}
) {
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  await waitUntilSlot(
    w,
    Number(round.openedSlot) + ROUND_WINDOW_SLOTS,
    `round #${roundId} window (${ROUND_WINDOW_SLOTS} slots)`
  );
  const { request } = await drawRound(w, starId, roundId, treasury);
  say(`sealed round #${roundId} and bought its draw, randomness at ${request.toBase58()}`);
  if (opts.fulfill !== false) {
    await oraoFulfill(w, request);
    say(`ORAO landed the draw for round #${roundId}`);
  }
  return request;
}

/** Waits out the window without sealing, so a scenario can drive the draw itself. */
export async function waitOutWindow(w: World, starId: number, roundId: number) {
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  return waitUntilSlot(
    w,
    Number(round.openedSlot) + ROUND_WINDOW_SLOTS,
    `round #${roundId} window (${ROUND_WINDOW_SLOTS} slots)`
  );
}
