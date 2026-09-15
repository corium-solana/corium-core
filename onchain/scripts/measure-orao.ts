/**
 * Measure what ORAO actually charges, right now, on the configured cluster.
 *
 * Everything the oracle budget has to cover is observable on chain:
 *   request_fee            - NetworkState, ORAO can change this at will
 *   rent(PENDING_SIZE)     - paid up front by whoever signs request_v2
 *   rent(FULFILLED_SIZE)   - what stays locked in the account forever
 *
 * ORAO returns rent(PENDING) - rent(FULFILLED) to the request payer on
 * fulfill, so the unrecoverable cost of one roll is
 * request_fee + rent(FULFILLED_SIZE).
 *
 * Sizes are cross-checked against live accounts rather than trusted from the
 * docs. Run: yarn tsx scripts/measure-orao.ts
 */
import { PublicKey } from '@solana/web3.js';

import {
  ORAO_VRF_PROGRAM_ID,
  context,
  fetchOraoNetwork,
  parseArgs,
} from './lib';

const sol = (n: number | bigint) => (Number(n) / 1e9).toFixed(9);

// ORAO's published space calculation for the request account:
//   8 (anchor tag) + 1 (enum tag) + 32 (client) + 32 (seed)
//   + 4 (response vec len) + (32 (fulfiller) + 64 (randomness)) * 7
const PENDING_SIZE = 8 + 1 + 32 + 32 + 4 + 96 * 7;
// Fulfilled: 8 (tag) + 1 (enum) + 32 (client) + 32 (seed) + 64 (randomness)
const FULFILLED_SIZE = 8 + 1 + 32 + 32 + 64;

async function main() {
  const { connection } = context(parseArgs());
  const orao = await fetchOraoNetwork(connection);

  console.log(`network state  ${orao.networkState.toBase58()}`);
  console.log(`  request_fee  ${sol(orao.requestFee)} SOL`);
  console.log();

  // Trust the chain over the docs. Count accounts at each candidate size and
  // check that a real fulfilled account holds exactly its rent-exempt minimum
  // (i.e. ORAO really did hand the difference back).
  for (const [label, size] of [
    ['PENDING  ', PENDING_SIZE],
    ['FULFILLED', FULFILLED_SIZE],
  ] as const) {
    const hits = await connection.getProgramAccounts(ORAO_VRF_PROGRAM_ID, {
      dataSlice: { offset: 0, length: 0 },
      filters: [{ dataSize: size }],
    });
    const rent = await connection.getMinimumBalanceForRentExemption(size);
    console.log(`${label} ${size} bytes  rent ${sol(rent)}  live accounts ${hits.length}`);
    if (hits.length) {
      const one = await connection.getAccountInfo(hits[0].pubkey);
      const held = one?.lamports ?? 0;
      // Old accounts keep whatever the rent rate was when they were fulfilled;
      // rent-exempt balances are never re-normalised. A mismatch here is not a
      // bug, it is direct evidence that the rate has moved.
      const note =
        held === rent
          ? 'matches the current rate'
          : `was fulfilled at a rate of ${(held / (128 + size)).toFixed(0)} lamports/byte, now ${(
              rent /
              (128 + size)
            ).toFixed(0)}`;
      console.log(`  sample ${hits[0].pubkey.toBase58()}  holds ${sol(held)}  (${note})`);
    }
  }
  console.log();

  const rentPending = await connection.getMinimumBalanceForRentExemption(PENDING_SIZE);
  const rentFulfilled = await connection.getMinimumBalanceForRentExemption(FULFILLED_SIZE);
  const fee = Number(orao.requestFee);

  console.log('per roll');
  console.log(`  paid up front by the request payer  ${sol(fee + rentPending)} SOL`);
  console.log(`  returned to the payer on fulfill    ${sol(rentPending - rentFulfilled)} SOL`);
  console.log(`  UNRECOVERABLE                       ${sol(fee + rentFulfilled)} SOL`);
  console.log();
  console.log('Both inputs are cluster state, not constants: ORAO can reprice');
  console.log('request_fee, and the rent rate differs per cluster and has moved');
  console.log('before. The program must read these, never hardcode them.');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
