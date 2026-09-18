/**
 * Shared plumbing for the SOLDUST dev scripts.
 *
 * Everything is deliberately dependency-light and explicit: PDAs are derived
 * here rather than relying on Anchor's account resolution, because one of our
 * seeds is a hash the IDL cannot describe.
 */

import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

import { AnchorProvider, BN, Program, Wallet } from '@coral-xyz/anchor';
import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
} from '@solana/web3.js';
import { sha256 } from '@noble/hashes/sha256';
import * as dotenv from 'dotenv';

dotenv.config({ path: path.resolve(__dirname, '..', '.env') });
dotenv.config({ path: path.resolve(__dirname, '..', '..', '.env') });

function loadIdl(): any {
  const candidates = [
    path.resolve(__dirname, '../target/idl/soldust.json'),
    path.resolve(__dirname, '../../shared/chain/soldust.json'),
  ];
  for (const p of candidates) {
    if (fs.existsSync(p)) return JSON.parse(fs.readFileSync(p, 'utf8'));
  }
  throw new Error('soldust IDL not found (build the program or ship shared/chain/soldust.json)');
}

const IDL = loadIdl();

// ---------------------------------------------------------------- constants

export { LAMPORTS_PER_SOL, PublicKey, Keypair, BN, SystemProgram };

/** Index -> display name for `Star.stage`. Mirrors `STAGE_ORDER` in the app. */
export const STAGE_NAMES = [
  'PROTOSTAR',
  'MAIN SEQUENCE',
  'BLUE GIANT',
  'RED GIANT',
  'SUPERGIANT',
  'CRITICAL',
  'EVENT HORIZON',
];

/** MagicBlock `ephemeral-vrf`. Same address on devnet and mainnet-beta. */
export const VRF_PROGRAM_ID = new PublicKey(
  'Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz'
);

/** Mirrors `vrf::VRF_QUEUE`, which the program pins. */
export const VRF_QUEUE = new PublicKey(
  'Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh'
);

/** Mirrors `vrf::VRF_REQUEST_FEE`: the whole cost of a draw. */
export const DRAW_COST = 500_000;

const CONFIG_SEED = Buffer.from('config');
const VAULT_SEED = Buffer.from('vault');
const STAR_SEED = Buffer.from('star');
const PUSH_SEED = Buffer.from('push');
const ROUND_SEED = Buffer.from('round');
const PLAYER_SEED = Buffer.from('player');
const FEED_SEED = Buffer.from('feed');
const FEED_SHARE_SEED = Buffer.from('feed-share');
const VRF_IDENTITY_SEED = Buffer.from('identity');

// ------------------------------------------------------------------ helpers

export function u64le(value: bigint | number | BN): Buffer {
  const buf = Buffer.alloc(8);
  buf.writeBigUInt64LE(BigInt(value.toString()));
  return buf;
}

export function sol(lamports: bigint | number | BN): string {
  const n = Number(lamports.toString()) / LAMPORTS_PER_SOL;
  return `${n.toFixed(6)} SOL`;
}

export function ppb(value: number): string {
  return `${(value / 10_000_000).toFixed(4)}% (${value} ppb)`;
}

export function ts(unix: bigint | number | BN): string {
  const n = Number(unix.toString());
  return n === 0 ? '-' : new Date(n * 1000).toISOString();
}

/** `{ pending: {} }` (Anchor enum) -> `"pending"`. */
export function enumName(value: any): string {
  if (!value || typeof value !== 'object') return String(value);
  return Object.keys(value)[0];
}

export function isStarFinished(status: any): boolean {
  const n = enumName(status);
  return n === 'dead' || n === 'blackHole' || n === 'stalled';
}

/** Finished by the feeder route: a black hole, or a stall collapse. */
export function isStarHoleShared(status: any): boolean {
  const n = enumName(status);
  return n === 'blackHole' || n === 'stalled';
}

// Stall timeouts, mirroring `constants.rs`.
export const STALL_SECS = 7 * 24 * 60 * 60;
export const NURSERY_STALL_SECS = 24 * 60 * 60;

/**
 * Seconds until anyone may collapse this star, negative once it is due, `null`
 * when the question does not apply. Mirrors `Star::is_stalled`; the program is
 * the authority, this only decides whether to spend a transaction asking.
 */
export function stallCountdown(
  star: { status?: any; totalMass?: any; pendingPushes?: any; lastMassTs?: any },
  now = Math.floor(Date.now() / 1000)
): number | null {
  if (isStarFinished(star.status)) return null;
  if (Number(star.pendingPushes?.toString?.() ?? 0) > 0) return null;
  const mass = Number(star.totalMass?.toString?.() ?? 0);
  const window = mass > 1e9 ? STALL_SECS : NURSERY_STALL_SECS;
  return Number(star.lastMassTs?.toString?.() ?? 0) + window - now;
}

export function isStarClosed(star: { status?: any; totalMass?: any; pendingLamports?: any }): boolean {
  if (isStarFinished(star.status)) return true;
  const mass = Number(star.totalMass?.toString?.() ?? 0);
  const pending = Number(star.pendingLamports?.toString?.() ?? 0);
  return mass + pending >= 21 * 1e9;
}

// --------------------------------------------------------------- CLI parsing

export type Args = Record<string, string | boolean>;

export function parseArgs(argv = process.argv.slice(2)): Args {
  const out: Args = {};
  for (let i = 0; i < argv.length; i++) {
    const token = argv[i];
    if (!token.startsWith('--')) continue;
    const key = token.slice(2);
    const next = argv[i + 1];
    if (next === undefined || next.startsWith('--')) {
      out[key] = true;
    } else {
      out[key] = next;
      i++;
    }
  }
  return out;
}

// -------------------------------------------------------------------- setup

export function loadKeypair(file: string): Keypair {
  const inline = process.env.CRANK_WALLET_JSON;
  if (inline && inline.trim().startsWith('[')) {
    const raw = JSON.parse(inline);
    return Keypair.fromSecretKey(Uint8Array.from(raw));
  }
  const resolved = file.startsWith('~')
    ? path.join(os.homedir(), file.slice(1))
    : path.resolve(file);
  const raw = JSON.parse(fs.readFileSync(resolved, 'utf8'));
  return Keypair.fromSecretKey(Uint8Array.from(raw));
}

/**
 * The generated IDL types are keyed off a const IDL object; these scripts load
 * the IDL at runtime instead so they keep working after a rebuild without a
 * type regeneration step. That costs us the typed account namespace, so it is
 * widened here rather than sprinkling casts through every script.
 */
export type SoldustProgram = Omit<Program<any>, 'account'> & {
  account: Record<string, any>;
};

export interface Ctx {
  connection: Connection;
  wallet: Wallet;
  provider: AnchorProvider;
  program: SoldustProgram;
  programId: PublicKey;
  config: PublicKey;
  vault: PublicKey;
}

/** Helius and friends keep the API key on the query string; keep it on wss too. */
export function rpcToWs(url: string): string {
  if (url.startsWith('https://')) return `wss://${url.slice('https://'.length)}`;
  if (url.startsWith('http://')) return `ws://${url.slice('http://'.length)}`;
  return url;
}

/**
 * Build a provider + program from `--wallet`/`ANCHOR_WALLET` and
 * `--url`/`SOLANA_RPC_URL`.
 */
export function context(args: Args = {}): Ctx {
  const url =
    (args.url as string) ||
    process.env.SOLANA_RPC_URL ||
    'https://api.devnet.solana.com';
  const walletPath =
    (args.wallet as string) ||
    process.env.ANCHOR_WALLET ||
    '~/.config/solana/id.json';

  const wsEndpoint = rpcToWs(url);
  const connection = new Connection(url, { commitment: 'confirmed', wsEndpoint });
  const wallet = new Wallet(loadKeypair(walletPath));
  const provider = new AnchorProvider(connection, wallet, {
    commitment: 'confirmed',
    preflightCommitment: 'confirmed',
  });

  const idl = { ...IDL };
  if (process.env.SOLDUST_PROGRAM_ID) {
    idl.address = process.env.SOLDUST_PROGRAM_ID;
  }
  const program = new Program(idl as any, provider) as unknown as SoldustProgram;
  const programId = program.programId;

  return {
    connection,
    wallet,
    provider,
    program,
    programId,
    config: configPda(programId),
    vault: vaultPda(programId),
  };
}

// ---------------------------------------------------------------------- PDAs

export function configPda(programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync([CONFIG_SEED], programId)[0];
}

export function vaultPda(programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync([VAULT_SEED], programId)[0];
}

export function starPda(programId: PublicKey, starId: bigint | number | BN): PublicKey {
  return PublicKey.findProgramAddressSync([STAR_SEED, u64le(starId)], programId)[0];
}

export function playerPda(programId: PublicKey, wallet: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [PLAYER_SEED, wallet.toBuffer()],
    programId
  )[0];
}

export function feedPda(programId: PublicKey, starId: bigint | number | BN): PublicKey {
  return PublicKey.findProgramAddressSync([FEED_SEED, u64le(starId)], programId)[0];
}

export function feedSharePda(
  programId: PublicKey,
  starId: bigint | number | BN,
  wallet: PublicKey
): PublicKey {
  return PublicKey.findProgramAddressSync(
    [FEED_SHARE_SEED, u64le(starId), wallet.toBuffer()],
    programId
  )[0];
}

/**
 * The push PDA is scoped to the player, not to a global counter, so two people
 * pushing in the same slot can never collide on an address.
 */
export function pushPda(
  programId: PublicKey,
  player: PublicKey,
  clientSeed: Uint8Array
): PublicKey {
  return PublicKey.findProgramAddressSync(
    [PUSH_SEED, player.toBuffer(), Buffer.from(clientSeed)],
    programId
  )[0];
}

export function roundPda(
  programId: PublicKey,
  starId: bigint | number | BN,
  roundId: bigint | number | BN
): PublicKey {
  return PublicKey.findProgramAddressSync(
    [ROUND_SEED, u64le(starId), u64le(roundId)],
    programId
  )[0];
}

export const SLOT_HASHES = new PublicKey('SysvarS1otHashes111111111111111111111111111');

/**
 * Must match `vrf::round_seed` on chain: `sha256("soldust:round" || star_id_le ||
 * round_id_le || entropy || slot_le || slot_hash || cranker)`.
 *
 * Whoever calls `draw_round` derives this to pass the matching `seed_slot`.
 * Afterwards the seed is on the round account, so reading it back is both easier
 * and authoritative.
 */
export function roundSeed(
  starId: bigint | number | BN,
  roundId: bigint | number | BN,
  entropy: Uint8Array,
  slot: bigint | number,
  slotHash: Uint8Array,
  cranker: PublicKey
): Buffer {
  return Buffer.from(
    sha256(
      Buffer.concat([
        Buffer.from('soldust:round'),
        u64le(starId),
        u64le(roundId),
        Buffer.from(entropy),
        u64le(slot),
        Buffer.from(slotHash),
        cranker.toBuffer(),
      ])
    )
  );
}

/**
 * The newest `(slot, hash)` in the `SlotHashes` sysvar, parsed by hand exactly as
 * `vrf::slot_hash_at` does: a `u64` count then `(u64 slot, [u8; 32] hash)` entries,
 * most recent first.
 */
export async function recentSlotHash(
  connection: Connection
): Promise<{ slot: bigint; hash: Buffer }> {
  const info = await connection.getAccountInfo(SLOT_HASHES);
  if (!info || info.data.length < 48) throw new Error('SlotHashes sysvar unreadable');
  if (info.data.readBigUInt64LE(0) === 0n) throw new Error('SlotHashes sysvar is empty');
  return { slot: info.data.readBigUInt64LE(8), hash: info.data.subarray(16, 48) };
}

/**
 * Must match `vrf::roll_for`: `sha256("soldust:roll" || randomness || push_id_le)`.
 *
 * Defined over the 64-byte form, so a 32-byte oracle output goes through
 * `widen` first. Done here rather than left to callers, so nothing can quietly
 * roll against a short input and get an answer the chain disagrees with.
 */
export function rollFor(randomness: Uint8Array, pushId: bigint | number | BN): number {
  const digest = sha256(
    Buffer.concat([Buffer.from('soldust:roll'), widen(randomness), u64le(pushId)])
  );
  let acc = 0n;
  for (let i = 15; i >= 0; i--) acc = (acc << 8n) | BigInt(digest[i]);
  return Number(acc % 1_000_000_000n);
}

/**
 * This program's VRF request identity: `PDA(["identity"], soldust)`. Signed by
 * the program during `draw_round`; never created as an account.
 */
export function vrfIdentityPda(programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync([VRF_IDENTITY_SEED], programId)[0];
}

/**
 * The identity the VRF program signs callbacks into us with:
 * `PDA(["identity", soldust], vrf)`. Mirrors `vrf::callback_identity`.
 */
export function vrfCallbackIdentityPda(programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [VRF_IDENTITY_SEED, programId.toBuffer()],
    VRF_PROGRAM_ID
  )[0];
}

/**
 * Widen a 32-byte oracle output to the 64 bytes `rollFor` is defined over.
 * Mirrors `vrf::widen`. Anything already 64 bytes passes through, so draws
 * recorded under the old ORAO integration still replay.
 */
export function widen(randomness: Uint8Array | Buffer): Buffer {
  if (randomness.length === 64) return Buffer.from(randomness);
  const wide = Buffer.alloc(64);
  Buffer.from(randomness).copy(wide, 0, 0, 32);
  return wide;
}

// --------------------------------------------------------------- convenience

export function explorer(signature: string): string {
  return `https://explorer.solana.com/tx/${signature}?cluster=devnet`;
}

export function accountExplorer(address: PublicKey): string {
  return `https://explorer.solana.com/address/${address.toBase58()}?cluster=devnet`;
}

export function printSignature(label: string, signature: string): void {
  console.log(`${label}: ${signature}`);
  console.log(`  ${explorer(signature)}`);
}

/** Fetch an account, returning null instead of throwing when absent. */
export async function tryFetch(
  program: SoldustProgram,
  name: string,
  address: PublicKey
): Promise<any | null> {
  try {
    return await program.account[name].fetch(address);
  } catch {
    return null;
  }
}

export function stageName(index: number): string {
  return STAGE_NAMES[index] ?? `STAGE ${index}`;
}

export function die(message: string): never {
  console.error(`\nError: ${message}\n`);
  process.exit(1);
}
