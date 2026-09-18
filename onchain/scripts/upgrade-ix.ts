/**
 * Build the BPF loader `Upgrade` instruction for Squads.
 *
 * Squads' native Developers -> Program upgrade flow only asks for the program
 * and the buffer, and it makes the vault the spill account. This exists for the
 * tx builder path instead, because the buffer rent should return to whoever
 * paid it (the deployer, ~2.66 SOL) rather than to the vault.
 *
 *   yarn tsx scripts/upgrade-ix.ts
 *   SPILL=<pubkey> yarn tsx scripts/upgrade-ix.ts
 */
import {
  Connection,
  PublicKey,
  Transaction,
  TransactionInstruction,
  SYSVAR_RENT_PUBKEY,
  SYSVAR_CLOCK_PUBKEY,
} from '@solana/web3.js';
import bs58 from 'bs58';

const BPF_LOADER_UPGRADEABLE_PROGRAM_ID = new PublicKey(
  'BPFLoaderUpgradeab1e11111111111111111111111'
);

const PROGRAM = new PublicKey(
  process.env.PROGRAM_ID || 'CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S'
);
const BUFFER = new PublicKey(
  process.env.BUFFER || 'HUK5b89mZgK1RNEMxS9pkf2jKbpxy65x21ANf6FV31A3'
);
const SPILL = new PublicKey(
  process.env.SPILL || 'NibBaP5XG5peeNsqYog2gSdizyYCJbkiNL45q6XwYqX'
);
const RPC =
  process.env.SOLANA_RPC_URL || process.env.RPC_URL || 'https://api.mainnet-beta.solana.com';

const main = async () => {
  const conn = new Connection(RPC, 'confirmed');

  const [programData] = PublicKey.findProgramAddressSync(
    [PROGRAM.toBuffer()],
    BPF_LOADER_UPGRADEABLE_PROGRAM_ID
  );

  const info = await conn.getAccountInfo(PROGRAM);
  if (!info) throw new Error(`program ${PROGRAM.toBase58()} not found on ${RPC}`);
  const pd = await conn.getAccountInfo(programData);
  if (!pd) throw new Error('ProgramData not found');
  // ProgramData: 4-byte enum (3) + 8-byte slot + 1-byte option + 32-byte authority
  const authority = new PublicKey(pd.data.subarray(13, 45));

  const buf = await conn.getAccountInfo(BUFFER);
  if (!buf) throw new Error(`buffer ${BUFFER.toBase58()} not found`);
  // Buffer: 4-byte enum (1) + 1-byte option + 32-byte authority
  const bufferAuthority = new PublicKey(buf.data.subarray(5, 37));

  // Upgrade = variant 3, u32 little-endian, no payload.
  const data = Buffer.alloc(4);
  data.writeUInt32LE(3, 0);

  const keys = [
    { pubkey: programData, isSigner: false, isWritable: true },
    { pubkey: PROGRAM, isSigner: false, isWritable: true },
    { pubkey: BUFFER, isSigner: false, isWritable: true },
    { pubkey: SPILL, isSigner: false, isWritable: true },
    { pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
    { pubkey: SYSVAR_CLOCK_PUBKEY, isSigner: false, isWritable: false },
    { pubkey: authority, isSigner: true, isWritable: false },
  ];

  const ix = new TransactionInstruction({
    programId: BPF_LOADER_UPGRADEABLE_PROGRAM_ID,
    keys,
    data,
  });

  const { blockhash } = await conn.getLatestBlockhash();
  const tx = new Transaction({ feePayer: authority, recentBlockhash: blockhash }).add(ix);
  const message = tx.serializeMessage();

  console.log(`rpc               ${RPC}`);
  console.log(`program           ${PROGRAM.toBase58()}`);
  console.log(`program data      ${programData.toBase58()}`);
  console.log(`buffer            ${BUFFER.toBase58()}`);
  console.log(`spill (refund)    ${SPILL.toBase58()}`);
  console.log(`program authority ${authority.toBase58()}`);
  console.log(`buffer authority  ${bufferAuthority.toBase58()}`);
  if (!authority.equals(bufferAuthority)) {
    console.log('WARNING: buffer authority does not match program authority');
  }
  console.log();
  console.log(`loader            ${BPF_LOADER_UPGRADEABLE_PROGRAM_ID.toBase58()}`);
  console.log(`instruction data  ${data.toString('hex')}  (base58 ${bs58.encode(data)})`);
  console.log();
  console.log('accounts in order:');
  keys.forEach((k, i) => {
    const flags = [k.isWritable ? 'writable' : 'readonly', k.isSigner ? 'signer' : '']
      .filter(Boolean)
      .join(', ');
    console.log(`  ${i}  ${k.pubkey.toBase58()}  ${flags}`);
  });
  console.log();
  console.log('base58 transaction message (Squads tx builder import):');
  console.log(bs58.encode(message));
  console.log();
  console.log('base64 transaction message:');
  console.log(message.toString('base64'));
};

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
