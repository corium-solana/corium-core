# SOLDUST devnet testing

Everything below is copy-pasteable. Run it from `onchain/`.

The goal is to walk the full lifecycle by hand: deploy → first star → real push
with real VRF → resolve → kill a star → verify winner and prize → verify a
stale push auto-refunds → next star.

Budget roughly **3.5 SOL of devnet SOL** for the deployer wallet (the program
itself costs ~2.4 SOL to deploy) and ~0.5 SOL for each test wallet.

- [1. Tooling](#1-tooling)
- [2. Point the CLI at devnet](#2-point-the-cli-at-devnet)
- [3. Wallet](#3-wallet)
- [4. Devnet SOL](#4-devnet-sol)
- [5. Build](#5-build)
- [6. Check the program id](#6-check-the-program-id)
- [7. Deploy](#7-deploy)
- [8. Initialize](#8-initialize)
- [9. First star](#9-first-star)
- [10. Make a push](#10-make-a-push)
- [11. Watch the VRF request](#11-watch-the-vrf-request)
- [12. Resolve](#12-resolve)
- [13. Inspect state](#13-inspect-state)
- [14. A second wallet](#14-a-second-wallet)
- [15. The curve is compiled in](#15-the-curve-is-compiled-in)
- [16. Kill the star](#16-kill-the-star)
- [17. Verify the winner](#17-verify-the-winner)
- [18. Verify the prize payout](#18-verify-the-prize-payout)
- [19. Verify automatic refund of a stale push](#19-verify-automatic-refund-of-a-stale-push)
- [20. Next star](#20-next-star)
- [21. Reset / redeploy](#21-reset--redeploy)
- [Troubleshooting](#troubleshooting)

---

## 1. Tooling

```bash
solana --version     # need Agave 3.x or 4.x
rustc --version      # any recent stable
node --version       # 18+
anchor --version     # need 1.2.0
```

Install anything missing:

```bash
# Solana CLI
sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Anchor 1.2.0
cargo install --locked anchor-cli
```

Then the JS dependencies:

```bash
cd onchain
yarn install
```

Sanity check before spending anything - this runs fully offline:

```bash
yarn tsx scripts/selftest.ts
```

```
ok    seed derivation matches Rust
ok    IDL exposes all 10 instructions  ...
ok    request_push encodes  88 bytes of data
...
All offline checks passed.
```

---

## 2. Point the CLI at devnet

```bash
solana config set --url https://api.devnet.solana.com
solana config get
```

Optionally create `onchain/.env` so the scripts agree with the CLI:

```bash
cp .env.example .env
```

The public devnet RPC is heavily rate limited. If `yarn crank` starts throwing
429s, put a free Helius/QuickNode devnet URL in `SOLANA_RPC_URL`.

---

## 3. Wallet

Use your existing keypair, or make one:

```bash
solana-keygen new --outfile ~/.config/solana/id.json
solana address
```

This wallet is the program's **upgrade** authority on this cluster, and that is
the only authority in the system. Nothing is written into `Config` about it -
the account has no `authority` field, because no instruction would read one.
Game numbers are compiled in. On mainnet, after you have verified the deploy,
set upgrade authority to `None`
(`solana program set-upgrade-authority <PROGRAM_ID> --final`). Do not do that
on this devnet walkthrough.

---

## 4. Devnet SOL

Devnet caps airdrops at 2 SOL per request and rate limits hard.

```bash
solana airdrop 2
sleep 20
solana airdrop 2
solana balance
```

If the CLI faucet refuses, use the web faucet at
<https://faucet.solana.com> (paste your address, pick devnet).

You want **at least 3.5 SOL** before deploying.

---

## 5. Build

```bash
anchor build
```

Produces:

- `target/deploy/soldust.so` - the program
- `target/idl/soldust.json` - the IDL the scripts and frontend load
- `target/types/soldust.ts` - generated TS types

Run the pure-logic tests too (fast, no network):

```bash
cargo test
```

```
test result: ok. 13 passed; 0 failed
```

---

## 6. Check the program id

```bash
anchor keys list
```

```
soldust: 7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX
```

This must match `declare_id!` in `programs/soldust/src/lib.rs` and the entries
in `Anchor.toml`. If you generated a new keypair, sync and rebuild:

```bash
anchor keys sync
anchor build
```

The keypair is `program-keypair.json` (gitignored) and
`target/deploy/soldust-keypair.json` (what Anchor actually reads). If
`target/deploy/soldust-keypair.json` goes missing, restore it rather than
letting Anchor generate a new one:

```bash
cp program-keypair.json target/deploy/soldust-keypair.json
```

---

## 7. Deploy

```bash
anchor program deploy --provider.cluster devnet
```

```
Deploying cluster: https://api.devnet.solana.com
Upgrade authority: /Users/you/.config/solana/id.json
Deploying program "soldust"...
Program ID: 7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX

Writing metadata account...
 ├─ metadata: 9gZEf8NTvrob7HJ4E11TDbh9EAyqNA7J2z8F9D7hocXB
 ├─ program: 7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX
 └─ seed: idl
```

Always confirm with the chain rather than trusting the exit code:

```bash
solana program show 7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX --url devnet
```

```
Program Id: 7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX
Authority: <your wallet>
Data Length: 333928 (0x51868) bytes
```

If you see that, **the program is deployed and you are done with this step.**

### `Error: Failed to initialize IDL` - safe to ignore

After uploading the program, Anchor 1.x makes a second, independent attempt to
write the IDL into an on-chain metadata account. That upload is many small
transactions and the public devnet RPC frequently rate-limits it:

```
Writing metadata account...
[Error] The provided transaction plan failed to execute. ...
Error: Failed to initialize IDL
```

**This does not affect the program or anything in this guide.** Nothing here
reads the on-chain IDL - `scripts/lib.ts` loads `target/idl/soldust.json` from
disk. Check `solana program show` as above, and if the program is there, carry
straight on to step 8.

The on-chain copy is only useful so block explorers can decode your
instructions. To retry it on its own:

```bash
anchor idl init --filepath target/idl/soldust.json \
  7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX --provider.cluster devnet
```

It tends to succeed on a private RPC (`--provider.cluster <your devnet url>`).
If the first attempt already created the account, use `anchor idl upgrade`
instead of `init`, or `anchor idl close` to bin it and start over.

### Upgrading later

After a code change, `anchor build && anchor program deploy` again. On-chain
state is preserved.

---

## 8. Initialize

Creates `Config` and the vault, and funds the vault to rent exemption.

```bash
yarn initialize
```

```
program   7bc84CiXLzDXaWiVpmW7EaN4aZgVa63qhRHiEim4ZTDX
payer     <you>  (no authority is recorded)
treasury  <you>  (frozen; there is no instruction to move it)
config    DbPZ...
vault     Angt...
genesis   4f2a...   <- random visual seed for star #1

initialize: 5xk2...
create_first_star: 3mQ9...

Star #1 -> HXxn...
```

Pass `--treasury <pubkey>` to send protocol revenue somewhere else, or
`--genesis-seed <64 hex chars>` to pin star #1's look.

```bash
yarn show-config
```

---

## 9. First star

`yarn initialize` already did this. To confirm:

```bash
yarn show-current-star
```

```
== Star #1 (ALIVE) ==
address            HXxn...
visual seed        4f2a...
stage              0 PROTOSTAR
mass               0.000000 SOL
prize pool         0.000000 SOL
push room          1.000000 SOL (whole 0.010000 SOL steps)
nova chance        none - nursery, a push here is a hole ticket
successful pushes  0
pending pushes     0
```

Past 1 SOL that line becomes a range - the smallest legal push against current
mass, and the largest:

```
mass               1.000000 SOL
prize pool         0.975000 SOL
push room          20.000000 SOL (whole 0.010000 SOL steps)
nova at min push   0.9901% (9900990 ppb)
nova at max push   95.2381% (952380952 ppb)
```

If you ever initialize with `--skip-first-star`, the first star is created by
the same `initialize` script on a second run.

Optional: in another terminal, tail the event stream for the rest of this
walkthrough.

```bash
yarn watch
```

---

## 10. Make a push

```bash
yarn push --amount 0.05
```

```
player     <you>
balance    1.234 SOL
star       #1 HXxn...
amount     0.050000 SOL x1
overhead   PendingPush rent 0.0014224 SOL, back on close, plus the round's
           0.0017577 SOL if you open it, back once every member resolves. Randomness costs the player nothing.

push 1/1: 2Yh8...
  push account   9kLm...
  round #0       E8hJ...
```

This is the **only** signature the player gives, and the stake plus that rent is
the whole bill - there is no oracle prepayment. The transaction escrowed the
stake and joined round #0 - and because you opened that round, your `client_seed`
became its entropy, the player-supplied half of the seed it will be drawn against.
Randomness is bought later, once for the whole round, by whoever cranks.

Note that nothing has counted yet:

```bash
yarn show-current-star     # mass still 0, pending pushes 1
yarn show-config           # pending escrow 0.05 SOL, prizes owed 0
```

Useful flags: `--count 3` to fire several, `--star N` to be explicit.

---

## 11. Watch the round seal and draw

```bash
yarn show-pending
```

```
push 9kLm...
  star #1  push #0  round #0  PENDING
  player     <you>
  amount     0.050000 SOL
  requested  2026-09-04T22:10:03.000Z (slot 1234567)
  round      E8hJ... OPEN (1 members share one draw)
  vrf        not sealed yet - still taking entrants
```

The round is the unit of work. It walks `OPEN → CLOSED → REQUESTED`, and only the
last of those permits settlement:

```
  vrf        sealed, draw not bought yet          <- CLOSED
  randomness Ff3p...
  vrf        waiting for ORAO oracles             <- REQUESTED
  vrf        FULFILLED - ready to resolve         <- draw landed
```

`yarn resolve` drives all of that by default (`--no-rounds` to only settle). A
round seals after ~75 slots (~30s), or as soon as 24 members have joined.

Devnet fulfillment is usually a few seconds. You can also look at the
randomness account directly:

```bash
solana account Ff3p...   # substitute the address printed above
```

---

## 12. Resolve

Permissionless - note that this works from *any* wallet, not just the pusher.

```bash
yarn resolve
```

```
survived  push #0 star #1 0.050000 SOL: 4Rt7...
1 resolved, 0 still waiting on randomness.
```

Or run it continuously, the way a backend would:

```bash
yarn crank --interval 4 --auto-next-star
```

---

## 13. Inspect state

```bash
yarn show-current-star
```

```
mass               0.050000 SOL
prize pool         0.048430 SOL     <- 96.86%
push room          0.950000 SOL (whole 0.010000 SOL steps)
successful pushes  1
pending pushes     0
```

```bash
yarn show-pending --all
```

```
  star #1  push #0  round #0  SURVIVED
  roll       734512891 ppb vs threshold 0 ppb
```

A nursery feed settles with a zero threshold - that is what tells an indexer it
was a feed and not a last hit. Past 1 SOL the threshold is the push's share of
the mass it created.

```bash
yarn show-player
```

```
STARDUST           60         <- 0.05 SOL * 1000 + 10 flat
pushes successful  1
total pushed       0.050000 SOL
stars killed       0
```

```bash
yarn show-config
```

```
  pending escrow   0.000000 SOL
  prizes owed      0.042500 SOL
  protocol fees    0.007500 SOL     <- 15%
solvent            yes
```

---

## 14. A second wallet

```bash
mkdir -p wallets
solana-keygen new --no-bip39-passphrase --outfile wallets/bob.json
solana airdrop 1 $(solana address -k wallets/bob.json)
```

Every script takes `--wallet`:

```bash
yarn push --wallet wallets/bob.json --amount 0.05
yarn show-player --wallet wallets/bob.json
```

Resolve Bob's push from *your* wallet to prove resolution needs no player
signature:

```bash
yarn resolve
```

Make a third if you want to watch concurrency properly:

```bash
solana-keygen new --no-bip39-passphrase --outfile wallets/carol.json
solana airdrop 1 $(solana address -k wallets/carol.json)
```

Fire several pushes from different wallets before resolving any of them - they
all sit pending against the same star, exactly as intended.

---

## 15. The curve is compiled in

There is no `update_config`, and `Config` carries no copy of the numbers to
read. Nova chances, splits, and push bounds live in
`programs/soldust/src/game_config.rs` and exist only in the deployed binary.

On **devnet** you still hold upgrade authority, so a walkthrough that needs a
guaranteed kill is a throwaway rebuild, not a live retune:

```bash
# temporarily make `nova_ppb` return PPB in game_config.rs, then:
anchor build && anchor upgrade target/deploy/soldust.so \
  --program-id CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S \
  --provider.cluster devnet
```

Put the real ratio back and upgrade again when you are done. Note the odds are
**not** frozen onto a star the way the old stage table was - they are computed
from mass at settle time, so restoring the binary restores them immediately,
including for stars already running.

A guaranteed kill no longer needs a rebuild, though. A push that takes all the
room left to the hole is `(21 − mass)/21` to go nova, so on a fresh 1 SOL star
`yarn push --amount 20` is a 95.2% one-shot. Two of those and a rebuild is
rarely worth it.

On **mainnet** that door closes: set upgrade authority to `None` after you
have verified the deploy. Do not `--final` this program id while you are
still iterating on devnet.

---

## 16. Kill the star

With a 100% test binary from step 15, push twice from two different wallets and **do not
resolve yet**:

```bash
yarn push --wallet wallets/bob.json   --amount 0.05
yarn push --wallet wallets/carol.json --amount 0.05
yarn show-pending          # two PENDING pushes on star #1
```

Now resolve. Settlement follows `push_id` order, not who clicked first. The
earlier request kills the star; the later one finds it already dead:

```bash
yarn resolve
```

```
killed    push #1 star #1 0.050000 SOL: 8Kp2...
  *** STAR #1 DESTROYED by <bob> ***
cancelled push #2 star #1 0.050000 SOL: 9Lq3...
  refunded 0.050000 SOL to <carol>
2 resolved, 0 still waiting on randomness.
```

That is the whole concurrency story in one command: the earliest requested
lethal push wins, and everything behind it refunds. A later VRF that fulfills
first cannot skip the queue - `resolve_push` rejects it with `PushOutOfOrder`
until the head has settled.

---

## 17. Verify the winner

```bash
yarn show-current-star
```

```
== Star #1 (DEAD) ==
mass               0.150000 SOL
successful pushes  3
cancelled pushes   1

-- death --
died               2026-09-04T22:31:12.000Z (slot 1234999)
star killer        <bob>
winning push       #1 9kLm...
final prize        0.127500 SOL
prize claimed      false
roll / threshold   1.2043% <  100.0000%
vrf output         a41c...   <- full 64-byte VRF proof output
next star created  false
```

```bash
yarn show-player --player <bob address>
```

```
stars killed       1
```

Anyone can verify the death independently: the `vrf output` is what ORAO signed
for that push's seed, and `roll < threshold` is the whole decision.

Note that the star is frozen. Any further push against it is rejected at
request time (`NotCurrentStar` / `StarNotAlive`), and anything already pending
refunds.

---

## 18. Verify the prize payout

Must be signed by the killer:

```bash
yarn claim-prize --star 1 --wallet wallets/bob.json
```

```
claim_prize star #1: 2Wq8...

prize        0.127500 SOL
balance      0.891234 SOL -> 1.018734 SOL
```

Check the guards:

```bash
# already claimed -> rejected
yarn claim-prize --star 1 --wallet wallets/bob.json

# wrong wallet -> rejected before it even builds the transaction
yarn claim-prize --star 1 --wallet wallets/carol.json
```

```bash
yarn show-config
```

`prizes owed` should have dropped by the prize, and `solvent` should still say
`yes`.

Protocol revenue, separately:

```bash
yarn withdraw-fees
```

It will refuse to send anything that would eat into escrow or unclaimed prizes.

---

## 19. Verify automatic refund of a stale push

Step 16 already showed this: the later push was cancelled and refunded inside
`yarn resolve`, with no action from that player. To see it in isolation, and to
prove a refund does **not** need randomness at all:

```bash
# star #2 must be alive for this - see step 20 first if needed
# use the 100% test binary from step 15 if you need a guaranteed kill
yarn push --wallet wallets/carol.json --amount 0.05    # push A
yarn push --wallet wallets/bob.json   --amount 0.05    # push B

# B cannot skip A while the star is alive:
yarn show-pending                                       # copy both push addresses
yarn resolve --push <B push address>                    # PushOutOfOrder

# Resolve A first (kills). Then B refunds with no VRF required.
solana balance $(solana address -k wallets/bob.json)
yarn resolve --push <A push address>
yarn resolve --push <B push address>
solana balance $(solana address -k wallets/bob.json)
```

```
killed    push #0 star #2 0.050000 SOL: ...
cancelled push #1 star #2 0.050000 SOL: 6Jm1...
  refunded 0.050000 SOL to <bob>
```

Bob's balance goes back up by the full 0.05 SOL. He never signed anything
and never had to claim.

Things worth confirming here:

- `yarn show-pending --all` shows the push as `CANCELLED`, and resolving it a
  second time fails with `PushAlreadyResolved` - no double refund.
- The refund path does not read randomness. If ORAO had never fulfilled B's
  round, it would *still* refund after A killed the star. And if the star had
  stayed alive, `expire_round` would have released the round after the timeout.
  Escrow cannot be stranded by a stuck oracle either way.
- `yarn show-config` - `pending escrow` back to 0, and the dead star's prize
  pool never moved.

Reclaim the rent on finished push accounts whenever you like:

```bash
yarn resolve --close
```

---

## 20. Next star

Permissionless - run it from any wallet:

```bash
yarn create-next-star --wallet wallets/carol.json
```

```
create_next_star #2: 4Nn7...

Star #2 <address>
visual seed 91be...
(derived from star #1's death randomness)
```

```bash
yarn show-current-star      # #2, ALIVE, PROTOSTAR, mass 0
yarn show-current-star --star 1   # #1 still DEAD, permanent, untouched
```

Things worth confirming:

- Calling it twice fails (`NextStarAlreadyCreated` / account already in use).
- Star #1's record is unchanged apart from the `next_star_created` latch.
- Star #2 starts at zero mass. Nothing rolled forward from #1.
- Star #2's seed is unpredictable - it comes from a VRF output nobody knew
  before #1 died.

Put the real curve back (restore `game_config.rs` and upgrade) and keep playing:

```bash
yarn push --amount 0.05 --count 5
yarn crank --auto-next-star
```

---

## 21. Reset / redeploy

**Code change, keep state.** Ordinary upgrade:

```bash
anchor build && anchor program deploy --provider.cluster devnet
```

**Change the shape of an account.** Account layouts are not migrated. Deploy
under a fresh program id (below) rather than trying to upgrade into a different
`Config` or `Star` layout.

**Full wipe - new program id, empty state:**

```bash
solana-keygen new --no-bip39-passphrase --force --outfile program-keypair.json
cp program-keypair.json target/deploy/soldust-keypair.json
anchor keys sync          # rewrites declare_id! and Anchor.toml
anchor build
anchor program deploy --provider.cluster devnet
yarn initialize
```

> Before running `anchor keys sync`, check `anchor keys list` actually shows the
> key you intend. It reads `target/deploy/soldust-keypair.json`, so if that file
> was regenerated or `CARGO_TARGET_DIR` is pointing somewhere unexpected, sync
> will happily rewrite `declare_id!` to the wrong program and orphan your
> deployment.
>
> A failed deploy can also leave a `target/deploy/soldust-upgrade-buffer.json`
> behind. Check whether it holds real SOL and reclaim it if so:
>
> ```bash
> solana program show --buffers --url devnet
> solana program close <BUFFER_ADDRESS> --url devnet   # only if one is listed
> ```

**Recover the SOL from an old deployment.** Note this burns the program id
permanently - a closed program can never be redeployed at that address:

```bash
solana program close <OLD_PROGRAM_ID> --bypass-warning
```

**Retune.** Change `game_config.rs`, rebuild, and upgrade. There is no live
config instruction. On mainnet, after `--final`, even this is gone.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Config not found` | Run `yarn initialize`. |
| `vrf waiting for ORAO oracles` forever | Check the randomness account exists (`solana account <addr>`). ORAO only runs on devnet and mainnet - there is no localnet deployment. |
| `RandomnessNotReady` from `resolve` | Fulfillment has not landed yet. Harmless, retry. |
| `PushOutOfOrder` from `resolve` | An earlier `push_id` on this live star is still pending. Resolve the head first (or wait for the crank). After death, refunds can go in any order. |
| `ORAO network state ... not found` | You are pointed at localnet or mainnet-beta. `solana config set --url https://api.devnet.solana.com`. |
| `custom program error: 0x1` on push | Not enough SOL. A push needs the amount plus ~0.0012 SOL of push rent (and ~0.0016 once for the `Player` account). Nothing for randomness. |
| `account already in use` on push | Astronomically unlikely `client_seed` reuse - just push again. |
| `NotCurrentStar` | The star died while you were typing. `yarn show-current-star`. |
| `StarNotStalled` from a collapse | The star gained mass inside its stall window (7 days, or 24 hours in the nursery). Only a genuinely idle star can be collapsed; the crank retries on its own once one is. |
| `StarQueueNotEmpty` from a collapse | Pushes are still queued. Clear them first - `expire_round` once the round has aged out, then `resolve_push` per member. Neither needs the oracle. |
| `ConstraintSeeds` on the `round` account | Your `star.current_round` was stale - the round sealed between your read and your send. Re-read the star and push again. |
| `RoundNotCloseable` / `RoundNotOpen` from the crank | Another crank sealed it first, or the window has not elapsed. Harmless. |
| `VrfAlreadyRequested` from the crank | Two cranks raced to buy the same round's draw. Harmless; one of them won. |
| `RoundNotRequested` from `resolve` | The round has not been drawn yet (or was voided). Run `yarn resolve` without `--no-rounds`, or wait for the crank. |
| `RoundNotExpired` from `expire_round` | Either the timeout has not elapsed, or the draw landed after all - in which case the round should settle normally, not void. |
| 429s from the RPC | Public devnet rate limit. Set `SOLANA_RPC_URL` to a private devnet endpoint. |
| `Error: Failed to initialize IDL` after deploy | Harmless. The program deployed; only the optional on-chain IDL copy failed. See [step 7](#7-deploy). |
| Deploy fails with insufficient funds | The program needs ~2.4 SOL. Airdrop more, or use <https://faucet.solana.com>. |
| `anchor build` regenerated a different program id | `cp program-keypair.json target/deploy/soldust-keypair.json`, then `anchor keys sync && anchor build`. |
