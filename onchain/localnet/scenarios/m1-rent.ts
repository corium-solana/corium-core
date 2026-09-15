/**
 * M-1 (fixed) - what a push actually costs when it does not roll, and who pays it.
 *
 * The bug: `request_push` opens up to four accounts with `init_if_needed`, and only
 * one of them - the push itself - had a `close_*` instruction. The `Round` account
 * in particular is created by whoever happens to be the batch's first member and
 * was never closed by anything, so that one arbitrary player silently subsidised
 * everyone else who joined. On a 0.01 SOL push that premium was a fifth of the
 * stake, for an account they had no idea they had opened.
 *
 * The fix records `Round.opened_by` and adds a permissionless
 * `close_round_account` that hands the rent back once every member has resolved.
 * This measures the real figure per role, on fresh wallets, through a full refund
 * and cleanup - and prints the numbers the fee documents have to match.
 */

import { Keypair } from '@solana/web3.js';

import {
  ROUND_EXPIRY_SLOTS,
  act,
  assert,
  bootstrap,
  closeRoundAccount,
  conclude,
  connect,
  expireRound,
  head,
  mustFail,
  newPlayer,
  requestPush,
  resolvePush,
  say,
  showRound,
  sol,
  waitUntilSlot,
} from '../lib';

const STAKE = 40_000_000;

async function main() {
  head('M-1: a round\'s rent goes back to the member who opened it');
  const w = await connect();
  const treasury = Keypair.generate().publicKey;
  await bootstrap(w, treasury);

  act('two fresh wallets join one round: the first opens it, the second does not');
  const first = await newPlayer(w);
  const second = await newPlayer(w);
  const firstStart = await w.connection.getBalance(first.publicKey);
  const secondStart = await w.connection.getBalance(second.publicKey);

  const pFirst = await requestPush(w, 1, STAKE, first);
  const pSecond = await requestPush(w, 1, STAKE, second);
  const roundId = pFirst.roundId;
  say(`round #${roundId}: opener push_id=${pFirst.pushId}, joiner push_id=${pSecond.pushId}`);

  const opened: any = await w.program.account.round.fetch(w.round(1, roundId));
  assert(
    opened.openedBy.toString() === first.publicKey.toString(),
    'the round recorded its opener, so the rent has somewhere to go back to'
  );

  act('itemise the accounts the pushes created');
  const items: [string, any][] = [
    ['Round (opener pays, reclaimable via close_round_account)', w.round(1, roundId)],
    ['PendingPush (reclaimable via close_push)', pFirst.pda],
    ['Player stats (once per wallet, never closed)', w.playerStats(first.publicKey)],
    ['FeedShare (once per wallet per star, never closed)', w.feedShare(1, first.publicKey)],
    ['StarFeed (once per star, never closed)', w.starFeed(1)],
  ];
  const rents: Record<string, number> = {};
  for (const [label, key] of items) {
    const info = await w.connection.getAccountInfo(key);
    rents[label] = info!.lamports;
    say(`${sol(info!.lamports)}  ${info!.data.length} bytes  ${label}`);
  }
  const roundRent = rents['Round (opener pays, reclaimable via close_round_account)'];

  act('the round\'s rent cannot be swept while a member still needs it');
  const early = await mustFail('close_round_account with both members pending', () =>
    closeRoundAccount(w, 1, roundId)
  );
  assert(early === 'RoundNotDrained', 'refused with RoundNotDrained');

  act('void the round and take both refunds');
  const r: any = await w.program.account.round.fetch(w.round(1, roundId));
  await waitUntilSlot(w, Number(r.openedSlot) + ROUND_EXPIRY_SLOTS, 'round expiry');
  await expireRound(w, 1, roundId);
  await showRound(w, 1, roundId);
  await resolvePush(w, pFirst);

  const halfway = await mustFail('close_round_account with one member left', () =>
    closeRoundAccount(w, 1, roundId)
  );
  assert(halfway === 'RoundNotDrained', 'still refused with one member unresolved');

  await resolvePush(w, pSecond);
  say('both stakes returned in full');

  act('reclaim everything that is reclaimable');
  for (const p of [pFirst, pSecond]) {
    await w.program.methods
      .closePush()
      .accountsPartial({ pendingPush: p.pda, playerWallet: p.player.publicKey })
      .rpc();
  }
  say('close_push called for both');

  await closeRoundAccount(w, 1, roundId);
  const gone = await w.connection.getAccountInfo(w.round(1, roundId));
  assert(gone === null, 'close_round_account closed the drained round');

  act('the bill');
  const player = rents['Player stats (once per wallet, never closed)'];
  const share = rents['FeedShare (once per wallet per star, never closed)'];
  const feedTally = rents['StarFeed (once per star, never closed)'];
  const firstEnd = await w.connection.getBalance(first.publicKey);
  const secondEnd = await w.connection.getBalance(second.publicKey);
  const firstLoss = firstStart - firstEnd;
  const secondLoss = secondStart - secondEnd;
  say(`round opener  out of pocket: ${sol(firstLoss)}`);
  say(`round joiner  out of pocket: ${sol(secondLoss)}`);
  say(`the opener's remaining premium: ${sol(firstLoss - secondLoss)}`);
  say(`(the Round account's rent was ${sol(roundRent)}, now returned)`);

  assert(
    firstLoss === secondLoss,
    'opening a round now costs the same as joining one, to the lamport'
  );
  // The harness provider is the fee payer, so these balances are rent and
  // nothing else - which makes the remainder an exact identity, not an estimate.
  assert(
    firstLoss === player + share,
    `what is left is exactly the two accounts that persist by design (${sol(player)} + ${sol(share)}), so every reclaimable lamport came back`
  );

  act('what is genuinely left, and what the fee docs must say');
  say('these three are per-wallet or per-star, are reused by every later push,');
  say('and are the real answer to "what does a push that never rolls cost":');
  say(`  Player     ${sol(player)}  once ever, per wallet`);
  say(`  FeedShare  ${sol(share)}  once per star, per wallet`);
  say(`  StarFeed   ${sol(feedTally)}  once per star, whoever gets there first`);
  say(`first push on a brand-new star, worst case: ${sol(player + share + feedTally)}`);
  say(`every push after that on the same star: transaction fees only`);

  process.exit(conclude('M-1'));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
