# localnet - the audit findings, and the fixes pinned down

Every finding in the pre-mainnet audit, run against a real `solana-test-validator`
rather than argued on paper. Each scenario is named for the bug it came from and
its header comment records what that bug was; the assertions themselves are the
fixed behaviour, so a failure here means a fix regressed. Each one ends by either
draining a player's money back to them or proving that nothing can.

## Why there is a mock oracle

SOLDUST CPIs into MagicBlock's `ephemeral-vrf` at
`Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz`, and that address is compiled into
the program. MagicBlock is push-based: soldust files a request against the oracle
queue, and the oracle network later calls back into soldust's
`consume_randomness`, signing with `PDA(["identity", soldust], vrf)`. Nothing
outside the VRF program can produce that signature and `consume_randomness`
accepts nothing else, so without standing in for the VRF program there is no way
to make a draw land at all - and every scenario downstream of a draw would have
nothing to assert on.

So `mock-magicblock-vrf/` is a stand-in loaded at MagicBlock's real address via
`solana-test-validator --bpf-program`, which places a program at an arbitrary
address without needing that address's keypair. The address is the whole point:
the callback identity is a PDA derived *underneath* it, so only a program sitting
there can sign a draw soldust will accept. The mock exists to hold that key, not
to hold a layout. The ORAO mock it replaces was the other way round - soldust
parsed ORAO's randomness account at hardcoded byte offsets, so that mock had to
reproduce the layout, write the account, and be able to corrupt it. Nothing
parses a foreign account any more, so `mock_corrupt` and the four damage modes it
simulated are gone with it.

Requests are parked in the oracle queue at the pinned address
`Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh`, exactly as the real program does,
because `draw_round`'s CPI passes exactly the five accounts the real MagicBlock
instruction takes - there is no room to hand the mock a scratch account of its
own, and adding one would mean soldust sending a request the real program would
refuse. `validator.sh` pre-loads that queue account from `vrf-queue.json`, owned
by the mock, because a PDA of the real VRF program cannot be created by a
stand-in for it. The mock also enforces MagicBlock's own rule that a fulfilment
may not land in the same slot as its request, so a scenario that tries to seal
and draw atomically discovers that here rather than in production.

On top of that it exposes what an oracle does off chain, in two forms.
`mock_fulfill` replays the request exactly as parked - callback program,
discriminator, accounts and args all come out of the queue item, never from
whoever calls it - and that is the honest path, the one that proves soldust
accepts a draw only as a signed callback. `mock_fulfill_unpinned` ignores the
parked accounts and lets the caller aim a draw at any round. It has no
counterpart in the real program, which reads those accounts off the queue item:
an outsider can never do this, and only MagicBlock could, by shipping a program
upgrade. It is there so the suite can state on the record what a *compromised*
oracle can and cannot do to soldust, which is also how the `c2` assertions below
have to be read.

## Running it

```bash
# once
cd mock-magicblock-vrf && cargo build-sbf --sbf-out-dir ./out && cd ..
cd .. && anchor build            # produces target/deploy/soldust.so

# one scenario, on a fresh validator
./run.sh localnet/scenarios/c1-stranded-cursor.ts

# everything
./run-all.sh
```

The mock is built on demand: `validator.sh` runs that same `cargo build-sbf`
itself if `mock-magicblock-vrf/out/` has no `.so` in it, so the first line above
only saves a wait on the first boot.

`validator.sh` sets `--ticks-per-slot 8`, which shortens a slot to about 50ms.
`ROUND_EXPIRY_SLOTS` is 750, so the expiry waits take ~55 seconds instead of five
minutes.

One switch:

- `SOLDUST_SO=/path/to/soldust.so` - load a different build. Point it at a pre-fix
  binary to confirm the fix is what moved, rather than the assertions.

`c3-stalled-star.ts` is the one scenario that needs a build of its own, because
the timeout it waits out ships as a week of cluster time:

```bash
./build-short-stalls.sh    # writes localnet/soldust-short-stalls.so, then
                           # rebuilds the shipped artifact
```

`run-all.sh` runs that for you whenever the artifact is missing *or* older than
any file under `programs/soldust/src`, and skips the scenario rather than failing
the suite if the build does not work. The staleness check is not defensive
housekeeping: during the MagicBlock migration a copy left in the tree went stale,
and c3 spent a suite run reporting a failure that belonged to the previous
oracle. The shipped week/day values are asserted by a `state.rs` unit test that
only compiles when the feature is off, so the short-fuse build can never be the
one that passes review.

All 11 scenarios pass as of the MagicBlock migration, alongside the 50 program
unit tests and the offline `yarn selftest`.

## The scenarios

| file | what it pins down |
| --- | --- |
| `00-smoke.ts` | baseline: push, seal, draw, settle. If this fails the harness is broken, not the program. |
| `c1-stranded-cursor.ts` | **C-1** an out-of-order refund used to move `settle_cursor` past a pending push and strand it, breaking the settle path for everyone behind it. The refund branch now respects the queue, so the batch drains from the head and every member gets out. |
| `c2-unreadable-vrf.ts` | **C-2** `expire_round` read the ORAO account purely to confirm the draw had *not* landed, and that read returned `Err` - not "no draw" - for a wrong owner, a wrong discriminator, an unknown tag, a short account or a seed mismatch. So a round whose ORAO account could not be parsed could neither void nor settle, and an oracle that answered wrongly, or shipped an upgrade moving the layout, took the escrow with it. The migration deleted the failure class rather than fixing it: nothing parses a foreign account, so the four damage shapes are no longer constructible and the scenario now pins the callback surface that replaced them. A stranger calling `consume_randomness` with its own keypair is refused (`InvalidVrfCallbackIdentity`); the real oracle-signed callback then lands on that same round; a second, differing draw for the same round is refused (`RoundNotRequested`) and the first draw stands; an all-zero draw is refused (`ZeroRandomness`); a round the oracle never answers voids on slots alone and refunds in full; and a draw arriving *after* expiry cannot revive the round into a settlement. The replay, zero and post-expiry legs go through `mock_fulfill_unpinned`, so they record what a compromised oracle cannot do, not what an outsider can reach. |
| `c3-stalled-star.ts` | **C-3** refunds get every stake out of a broken round, but they land no mass - so a star whose oracle never came back used to sit Alive forever holding a pot with no claimant, and no successor could be born past it. `collapse_stalled_star` now finishes any star that has gone a week without gaining mass (a day in the nursery) once its queue is empty: feeders take back the prize-side value of their own feeds and the settled pot endows the next star. Needs the `short-stalls` build, which `run-all.sh` produces. |
| `h1-frontrun-seed.ts` | **H-1** `close_round` published the round's seed one transaction before `request_round_vrf` spent it, and ORAO's `request_v2` was open to anyone - so a stranger could occupy the randomness address the round had committed to, after which the draw failed its emptiness pre-check forever and the star could never settle a push. Seal and draw are one transaction now, and the scenario still pins that half: an Open round carries no seed at all, so there is no sealed-but-undrawn state to read. MagicBlock then removed the address to squat altogether, so the rest of the scenario proves the gate that superseded the fix - a stranger cannot file a request naming soldust as its callback. An unsigned identity is refused (`MissingRequiredSignature`), and an identity the stranger actually holds does not derive from soldust and is refused (`InvalidSeeds`); both die in simulation, so the stranger never even pays a request fee. The round, untouched, then seals and settles normally. |
| `h2-initialize-race.ts` | **H-2** `initialize` had no signer gate, so the first caller owned the treasury permanently. It is now gated on the program's live upgrade authority, and the treasury it writes is unrevisable. |
| `h3-draw-race.ts` | **H-3** `round.entropy` used to be folded again on every arrival, so any push landing between a crank reading a round and its draw executing moved the ORAO address and reverted the draw - needing no attacker, only traffic, with the batch expiring into refunds if nobody won the gap. It is now committed once by the member who opened the round: the scenario derives the seed before a second member joins and finds the round committing exactly that seed afterwards. No address follows from the seed under MagicBlock, so the revert is gone and what the frozen field buys is a seed the crank and any client can re-derive and find unmoved - but the race the scenario constructs is the same one. |
| `m1-rent.ts` | **M-1** the `Round` account's rent was charged to one arbitrary member and never returned. `close_round_account` now hands it back to `Round.opened_by`, so opening a round costs the same as joining one - and the scenario reads every rent figure the fee docs quote off a live validator. |
| `m3-dead-star-draw.ts` | **M-3** a finished star used to seal and draw a round whose members `resolve_push` was going to refund regardless. `draw_round` now requires a live star, so the float buys no draw nobody reads and those members exit immediately, rent included. |
| `m4-float-floor.ts` | **M-4** `withdraw_protocol_fees` is permissionless so the house need not be online to be paid, but with no floor a stranger could sweep the float `draw_round` reimburses cranks from, leaving third-party cranking unpaid. Collection now stops at `DRAW_FLOAT_FLOOR`, and only the treasury signing for itself can go below. |
| `regress-death-drain.ts` | regression for the C-1 fix: a supernova mid-queue still drains everyone behind it, in order, with the prize paid. Uses a ground draw to force the kill. |

## Note

Nothing here is deployed and nothing here is part of the program. The mock is its
own cargo workspace precisely so it cannot affect the audited build's lockfile or
release profile.
