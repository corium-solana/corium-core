/**
 * Counts the RPC requests a busy round costs the browser.
 *
 * The pending sweep used to fan out one `getAccountInfo` per open push, so a
 * full 24-member round cost 24 requests every time it ran - past the proxy's
 * per-IP budget from a single tab. This asserts the batched path and pins the
 * account names the decoders rely on, which are easy to get wrong: Anchor
 * camel-cases the IDL when it builds a `Program`, and a wrong name fails as a
 * silent `null` rather than an error.
 *
 * Run: node onchain/scripts/rpc-budget-check.mjs
 */

import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

import idl from '../../shared/chain/soldust.json' with { type: 'json' };
import { decodeStarAccount, fetchPendingPushes } from '../../shared/chain/program.js';

const { AnchorProvider, Program } = anchor;

let failures = 0;
function check(label, got, want) {
  const ok = got === want;
  if (!ok) failures += 1;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${label}: ${got}${ok ? '' : ` (want ${want})`}`);
}

const wallet = {
  publicKey: Keypair.generate().publicKey,
  signTransaction: async (t) => t,
  signAllTransactions: async (t) => t,
};
const program = new Program(
  { ...idl },
  new AnchorProvider(new Connection('http://127.0.0.1:8899'), wallet, {
    commitment: 'confirmed',
  })
);

/** Buffer for an account of `name`, discriminator set so decoding succeeds. */
function accountBuffer(idlName, coderName) {
  const spec = idl.accounts.find((a) => a.name === idlName);
  const buf = Buffer.alloc(program.coder.accounts.size(coderName));
  Buffer.from(spec.discriminator).copy(buf, 0);
  return buf;
}

// ------------------------------------------------------- decoders still decode

const star = decodeStarAccount({ program }, accountBuffer('Star', 'star'));
check('star decodes from a websocket payload', star !== null, true);
check('star exposes pendingLamports', star ? 'pendingLamports' in star : false, true);
check('star exposes totalMass', star ? 'totalMass' in star : false, true);

// Why the decoders hold their own name constants: the IDL spelling is not what
// the Program's coder answers to, and the failure is a throw the caller turns
// into `null` - indistinguishable from "no star yet".
let idlSpellingWorks = true;
try {
  program.coder.accounts.decode('Star', accountBuffer('Star', 'star'));
} catch {
  idlSpellingWorks = false;
}
check("the IDL's own spelling is rejected by the coder", idlSpellingWorks, false);

// ------------------------------------------------- one request for whole round

const pushBuf = accountBuffer('PendingPush', 'pendingPush');
const keys = Array.from({ length: 24 }, () => Keypair.generate().publicKey);

const calls = [];
const ctx = {
  program,
  connection: {
    async getMultipleAccountsInfo(pks, commitment) {
      calls.push({ n: pks.length, commitment });
      // Last one closed: a resolved push whose account is gone.
      return pks.map((_, i) => (i === pks.length - 1 ? null : { data: pushBuf }));
    },
  },
};

const found = await fetchPendingPushes(ctx, keys);
check('requests for a 24-push round', calls.length, 1);
check('accounts asked for in that request', calls[0]?.n, 24);
check('read at confirmed', calls[0]?.commitment, 'confirmed');
check('rows returned', found.size, 24);
check('closed push reported as absent', found.get(keys[23].toBase58()), null);
check(
  'open push decoded with camelCase fields',
  'requestedTs' in (found.get(keys[0].toBase58()) ?? {}),
  true
);

// A push nobody asked about must not appear, or callers would settle it blind.
check('unasked key absent from the map', found.has(Keypair.generate().publicKey.toBase58()), false);

// ------------------------------------------------------------------- chunking

calls.length = 0;
await fetchPendingPushes(ctx, Array.from({ length: 250 }, () => Keypair.generate().publicKey));
check('250 pushes chunk into requests of <=100', calls.length, 3);
check('no chunk exceeds the batch cap', Math.max(...calls.map((c) => c.n)) <= 100, true);

calls.length = 0;
await fetchPendingPushes(ctx, []);
check('empty queue costs nothing', calls.length, 0);

// ------------------------------------------------- a non-PendingPush account

calls.length = 0;
const foreign = {
  program,
  connection: {
    async getMultipleAccountsInfo(pks) {
      calls.push(pks.length);
      return pks.map(() => ({ data: accountBuffer('Star', 'star') }));
    },
  },
};
const odd = await fetchPendingPushes(foreign, [new PublicKey(keys[0])]);
check('an account of the wrong type is not reported absent', odd.size, 0);

console.log(failures ? `\n${failures} check(s) failed` : '\nall checks passed');
process.exit(failures ? 1 : 0);
