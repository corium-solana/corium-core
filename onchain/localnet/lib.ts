/**
 * Harness for the local-validator reproductions.
 *
 * Deliberately self-contained: it talks to the program through the IDL and
 * nothing else, so a scenario passing or failing says something about the
 * program rather than about `shared/chain`. The mock VRF instructions are
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

export const VRF_PROGRAM_ID = new PublicKey('Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz');
/** The queue soldust pins. Its address is all soldust knows about it. */
export const VRF_QUEUE = new PublicKey('Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh');
export const BPF_LOADER_UPGRADEABLE = new PublicKey(
  'BPFLoaderUpgradeab1e11111111111111111111111'
);
/**
 * MagicBlock's real request fee (`VRF_LAMPORTS_COST`), so the economics gate
 * behaves as it will live. Unlike ORAO there is no second, unrecoverable rent
 * term: this is the whole cost of a draw.
 */
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

/**
 * Runtime failures that never carry an Anchor error code, mapped to the name
 * the `ProgramError` variant goes by.
 *
 * Needed because the checks guarding the oracle boundary are enforced by the
 * runtime and by the VRF program, not by Anchor: a missing identity signature
 * or an identity that does not derive from soldust fails before any
 * `#[account]` constraint runs. Without this a scenario would have to match on
 * a prose sentence.
 */
const RUNTIME_ERRORS: [RegExp, string][] = [
  [/missing required signature/i, 'MissingRequiredSignature'],
  [/seeds do not result in a valid address/i, 'InvalidSeeds'],
  [/insufficient account keys/i, 'NotEnoughAccountKeys'],
  [/instruction spent from the balance of an account it does not own/i, 'ExternalAccountLamportSpend'],
  [/account is not owned by|IllegalOwner/i, 'IllegalOwner'],
  [/privilege escalation/i, 'PrivilegeEscalation'],
];

/** Anchor error code name, however the RPC chose to wrap it. */
export function errCode(e: any): string {
  const direct = e?.error?.errorCode?.code;
  if (direct) return direct;
  const logs: string[] = e?.logs ?? e?.transactionLogs ?? [];
  for (const line of logs) {
    const m = line.match(/Error Code: (\w+)/);
    if (m) return m[1];
  }
  const text = String(e?.message ?? e);
  const m = text.match(/Error Code: (\w+)/);
  if (m) return m[1];
  const haystack = `${text}\n${logs.join('\n')}`;
  for (const [pattern, name] of RUNTIME_ERRORS) {
    if (pattern.test(haystack)) return name;
  }
  return text.slice(0, 160);
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
    vrfQueue: VRF_QUEUE,
    /**
     * `PDA(["identity"], soldust)`. Soldust signs its *requests* with this, and
     * MagicBlock refuses a request whose identity does not derive from the
     * callback program it names - which is what makes it impossible for anyone
     * but soldust to aim a draw at a soldust round.
     */
    vrfIdentity: pda([Buffer.from('identity')]),
    /**
     * `PDA(["identity", soldust], vrf)`. The VRF program signs its *callbacks*
     * with this, and `consume_randomness` accepts nothing else.
     */
    vrfCallbackIdentity: PublicKey.findProgramAddressSync(
      [Buffer.from('identity'), programId.toBuffer()],
      VRF_PROGRAM_ID
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

// --------------------------------------------------- mock MagicBlock clients

const disc = (name: string) =>
  createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);

/**
 * Harness-only instruction tags on the mock. MagicBlock's own tags are small
 * integers in the low byte of a u64, so these cannot collide with a real one.
 */
const IX_MOCK_FULFILL = Buffer.from([0xf0, 0, 0, 0, 0, 0, 0, 0]);
const IX_MOCK_FULFILL_UNPINNED = Buffer.from([0xf1, 0, 0, 0, 0, 0, 0, 0]);

/**
 * A 32-byte draw as `roll_for` sees it: right-padded to 64.
 *
 * Mirrors `vrf::widen`. Anything that predicts a roll has to go through this,
 * because grinding a 64-byte buffer directly would search a space the chain
 * can never produce.
 */
export function widen(randomness: Buffer | Uint8Array): Buffer {
  const wide = Buffer.alloc(64);
  Buffer.from(randomness).copy(wide, 0, 0, 32);
  return wide;
}

/**
 * Nothing to do: `validator.sh` pre-loads the queue at its pinned address,
 * owned by the mock, because that is where the mock stores requests.
 *
 * Kept as a named step so `bootstrap` still reads as a list of the things a
 * world needs. Replaces `oraoInit`, which had to publish a fee and a treasury
 * for soldust to read - neither exists under MagicBlock, where the fee is a
 * constant and the queue is pinned by address.
 */
export async function vrfInit(_w: World) {}

/**
 * What the oracle network does off-chain, then submits: land the draw.
 *
 * The mock replays the request it parked - callback program, discriminator,
 * accounts and args all come from there, not from here - and signs the CPI with
 * `PDA(["identity", soldust], vrf)`. So this call proves the real thing: that
 * soldust accepts a draw *only* when it arrives as a callback carrying a
 * signature nothing outside the VRF program can produce.
 *
 * `fill` is a 32-byte draw; the default is recognisable in logs but otherwise
 * arbitrary.
 */
export async function vrfFulfill(
  w: World,
  starId: number,
  roundId: number,
  seed: Buffer | Uint8Array,
  fill?: Buffer | Uint8Array
) {
  const randomness = Buffer.alloc(32, 0x11);
  if (fill) Buffer.from(fill).copy(randomness, 0, 0, 32);
  // MagicBlock refuses a fulfilment in the request's own slot, so a real oracle
  // never answers faster than this either.
  await waitUntilSlot(w, (await slot(w)) + 1, 'the oracle cannot answer in the request slot');
  const ix = new TransactionInstruction({
    programId: VRF_PROGRAM_ID,
    keys: [
      { pubkey: VRF_QUEUE, isSigner: false, isWritable: true },
      { pubkey: w.programId, isSigner: false, isWritable: false },
      // Not a signer here. It becomes one inside the CPI, which is the whole
      // point - a transaction cannot present this key.
      { pubkey: w.vrfCallbackIdentity, isSigner: false, isWritable: false },
      { pubkey: w.round(starId, roundId), isSigner: false, isWritable: true },
    ],
    data: Buffer.concat([IX_MOCK_FULFILL, Buffer.from(seed), randomness]),
  });
  return send(w, [ix], [w.payer]);
}

/**
 * A VRF program that has stopped honouring its own queue: deliver a draw to a
 * round of the caller's choosing, ignoring what the request recorded.
 *
 * Unreachable by an outsider against the real program, which reads its callback
 * accounts off the queue item. Only MagicBlock could do this, by shipping an
 * upgrade. It is here so the suite can state on the record what a compromised
 * oracle can and cannot do to soldust.
 */
export async function vrfFulfillUnpinned(
  w: World,
  round: PublicKey,
  fill: Buffer | Uint8Array
) {
  const randomness = Buffer.alloc(32);
  Buffer.from(fill).copy(randomness, 0, 0, 32);
  const ix = new TransactionInstruction({
    programId: VRF_PROGRAM_ID,
    keys: [
      { pubkey: w.programId, isSigner: false, isWritable: false },
      { pubkey: w.vrfCallbackIdentity, isSigner: false, isWritable: false },
      { pubkey: round, isSigner: false, isWritable: true },
    ],
    data: Buffer.concat([
      IX_MOCK_FULFILL_UNPINNED,
      randomness,
      disc('consume_randomness'),
    ]),
  });
  return send(w, [ix], [w.payer]);
}

/**
 * File a scoped randomness request against the VRF program directly, as a
 * stranger would, naming soldust as the callback program.
 *
 * The replacement for `oraoRequestDirect`. ORAO's `request_v2` was open to
 * anyone, which is what made squatting a round's randomness address possible;
 * MagicBlock requires the request to be signed by
 * `PDA(["identity"], callback_program)`, so this is the call that proves an
 * outsider cannot buy a draw on soldust's behalf. `identity` defaults to
 * soldust's real request identity - which the caller cannot sign for - and
 * `sign` controls whether it is even presented as a signer.
 */
export async function vrfRequestDirect(
  w: World,
  from: Keypair,
  opts: { identity?: PublicKey; sign?: Keypair; seed?: Buffer } = {}
) {
  const seed = opts.seed ?? Buffer.alloc(32, 0xab);
  const identity = opts.sign?.publicKey ?? opts.identity ?? w.vrfIdentity;
  const data = Buffer.concat([
    Buffer.from([10, 0, 0, 0, 0, 0, 0, 0]), // RequestRandomnessScoped
    seed,
    w.programId.toBuffer(),
    Buffer.from(new Uint32Array([8]).buffer),
    disc('consume_randomness'),
    Buffer.from(new Uint32Array([1]).buffer),
    w.round(1, 0).toBuffer(),
    Buffer.from([0, 1]),
    Buffer.from(new Uint32Array([0]).buffer),
  ]);
  const ix = new TransactionInstruction({
    programId: VRF_PROGRAM_ID,
    keys: [
      { pubkey: from.publicKey, isSigner: true, isWritable: true },
      { pubkey: identity, isSigner: !!opts.sign, isWritable: false },
      { pubkey: VRF_QUEUE, isSigner: false, isWritable: true },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: SYSVAR_SLOT_HASHES_PUBKEY, isSigner: false, isWritable: false },
    ],
    data,
  });
  const tx = new Transaction().add(ix);
  tx.feePayer = from.publicKey;
  const { blockhash } = await w.connection.getLatestBlockhash();
  tx.recentBlockhash = blockhash;
  tx.sign(...(opts.sign ? [from, opts.sign] : [from]));
  const sig = await w.connection.sendRawTransaction(tx.serialize());
  await w.connection.confirmTransaction(sig, 'confirmed');
  return sig;
}

/**
 * Call `consume_randomness` head-on, with a signer of the caller's choosing
 * standing in for the VRF identity.
 *
 * The forgery the callback model has to refuse. Defaults to an arbitrary
 * keypair; pass `as` to try a specific one.
 */
export async function consumeRandomnessDirect(
  w: World,
  starId: number,
  roundId: number,
  fill: Buffer | Uint8Array,
  opts: { as?: Keypair } = {}
) {
  const impostor = opts.as ?? Keypair.generate();
  const randomness = Buffer.alloc(32);
  Buffer.from(fill).copy(randomness, 0, 0, 32);
  return w.program.methods
    .consumeRandomness(Array.from(randomness))
    .accountsPartial({
      vrfIdentity: impostor.publicKey,
      round: w.round(starId, roundId),
    })
    .signers([impostor])
    .rpc();
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
 * The caller still derives the seed itself, but for a different reason than it
 * did under ORAO: nothing addressable follows from the seed any more, and the
 * mock needs it only to name the account it parks the request in. `seedSlot`
 * lets a scenario pin the slot to reproduce a specific seed; otherwise it takes
 * the newest one, which is what the real crank does.
 */
export async function drawRound(
  w: World,
  starId: number,
  roundId: number,
  opts: { cranker?: Keypair; seedSlot?: bigint; slotHash?: Buffer } = {}
) {
  const cranker = opts.cranker ?? w.payer;
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  const recent = await recentSlotHash(w);
  const seedSlot = opts.seedSlot ?? recent.slot;
  const slotHash = opts.slotHash ?? recent.hash;
  const seed = roundSeed(starId, roundId, round.entropy, seedSlot, slotHash, cranker.publicKey);

  await w.program.methods
    .drawRound(new BN(seedSlot.toString()))
    .accountsPartial({
      cranker: cranker.publicKey,
      config: w.config,
      vault: w.vault,
      star: w.star(starId),
      round: w.round(starId, roundId),
      slotHashes: SYSVAR_SLOT_HASHES_PUBKEY,
      vrfProgram: VRF_PROGRAM_ID,
      vrfIdentity: w.vrfIdentity,
      vrfQueue: w.vrfQueue,
      systemProgram: SystemProgram.programId,
    })
    .signers(cranker === w.payer ? [] : [cranker])
    .rpc();

  return { seed, seedSlot, round: w.round(starId, roundId) };
}

/** The mock's queue, for scenarios that want to look at what is in flight. */
export async function queuedSeeds(w: World): Promise<string[]> {
  const info = await w.connection.getAccountInfo(VRF_QUEUE);
  if (!info) return [];
  const out: string[] = [];
  for (let at = 8; at + 512 <= info.data.length; at += 512) {
    if (info.data[at] !== 0) out.push(info.data.subarray(at + 33, at + 65).toString('hex'));
  }
  return out;
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
  // No oracle account to name any more. Voiding is now purely a question of the
  // round's own status and how long it has sat there, which is why the ORAO
  // adapter's "is this account unreadable or merely unanswered" problem
  // disappeared with it.
  return w.program.methods
    .expireRound()
    .accountsPartial({
      cranker: w.payer.publicKey,
      star: w.star(starId),
      round: w.round(starId, roundId),
    })
    .rpc();
}

export async function resolvePush(w: World, p: Push) {
  const push: any = await w.program.account.pendingPush.fetch(p.pda);
  const starId = Number(push.starId);
  const roundId = Number(push.roundId);
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
 * covers what MagicBlock charges. Mirrors `Round::rake` and
 * `vrf::draw_cost_estimate`.
 *
 * Takes the world only so the signature survived the ORAO version, which had to
 * ask the chain for a rent-exemption figure. MagicBlock entombs no rent, so the
 * cost is now a constant and the bar sits roughly four times lower.
 */
export async function drawCost(_w: World) {
  return REQUEST_FEE;
}

export async function stakeNeededForDraw(w: World) {
  const cost = await drawCost(w);
  const raw = Math.ceil((cost * 10_000) / PROTOCOL_BPS);
  return Math.ceil(raw / PUSH_STEP) * PUSH_STEP;
}

// ------------------------------------------------------- forcing an outcome

/**
 * Mirrors `vrf::roll_for`: sha256("soldust:roll" ‖ draw ‖ push_id_le)[..16] mod 1e9.
 *
 * Always hashes the wide form, so a 32-byte oracle draw and the 64 bytes the
 * chain actually rolls over agree. Passing an already-widened buffer is a no-op.
 */
export function rollFor(randomness: Buffer | Uint8Array, pushId: number): number {
  const wide = randomness.length === 64 ? Buffer.from(randomness) : widen(randomness);
  const h = createHash('sha256')
    .update(Buffer.concat([Buffer.from('soldust:roll'), wide, u64(pushId)]))
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
    // 32 bytes, because that is all the oracle delivers. Grinding the 64-byte
    // form would search draws the chain can never produce, since `widen` zeroes
    // the upper half.
    const r = Buffer.alloc(32);
    r.writeUInt32LE(i, 0);
    r.writeUInt32LE(i ^ 0x5a5a5a5a, 28);
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
    const r = Buffer.alloc(32);
    r.writeUInt32LE(i, 0);
    r.writeUInt32LE(i ^ 0x5a5a5a5a, 28);
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
  await vrfInit(w);
  say(`mock VRF queue at ${VRF_QUEUE.toBase58()} (fee ${REQUEST_FEE})`);
  await initialize(w, treasury);
  await fundProtocol(w, 2 * LAMPORTS_PER_SOL);
  await createFirstStar(w);
  await feed(w, 1, FEED_MASS);
  const s = await showStar(w, 1, 'after nursery');
  return s;
}

/**
 * Wait out a round's window, seal-and-draw it, and optionally land the draw.
 *
 * Returns the seed, which is what a scenario needs to drive the mock later -
 * the ORAO version returned an account address, and there is no longer one.
 */
export async function sealAndDraw(
  w: World,
  starId: number,
  roundId: number,
  opts: { fulfill?: boolean; fill?: Buffer | Uint8Array } = {}
) {
  const round: any = await w.program.account.round.fetch(w.round(starId, roundId));
  await waitUntilSlot(
    w,
    Number(round.openedSlot) + ROUND_WINDOW_SLOTS,
    `round #${roundId} window (${ROUND_WINDOW_SLOTS} slots)`
  );
  const { seed } = await drawRound(w, starId, roundId);
  say(`sealed round #${roundId} and bought its draw, seed ${seed.toString('hex').slice(0, 16)}...`);
  if (opts.fulfill !== false) {
    await vrfFulfill(w, starId, roundId, seed, opts.fill);
    say(`the oracle landed the draw for round #${roundId}`);
  }
  return seed;
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
