# localnet - the audit findings, and the fixes pinned down

Every finding in the pre-mainnet audit, run against a real `solana-test-validator`
rather than argued on paper. Each scenario is named for the bug it came from and
its header comment records what that bug was; the assertions themselves are the
fixed behaviour, so a failure here means a fix regressed. Each one ends by either
draining a player's money back to them or proving that nothing can.

## Why there is a mock oracle

SOLDUST CPIs into ORAO VRF v2 at `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`,
and that address is compiled into the program along with the PDA seeds and the
byte offsets it reads out of the randomness account. ORAO does not exist on a
local validator, and its real program cannot be made to answer here because
fulfilment needs their oracles' keys.

So `mock-orao-vrf/` is a stand-in loaded at ORAO's real address via
`solana-test-validator --bpf-program`, which places a program at an arbitrary
address without needing that address's keypair. It reproduces exactly what
soldust looks at: the `request_v2` discriminator, the `RandomnessV2` account
discriminator, the 749-byte pending size, the field offsets, and `request_fee` at
offset 72 of the network state. On top of that it exposes what a real oracle does
outside the chain - `mock_fulfill` to land a draw on demand, and `mock_corrupt` to
simulate ORAO shipping an upgrade that moves the layout.

## Running it

```bash
# once
cd mock-orao-vrf && cargo build-sbf --sbf-out-dir ./out && cd ..
cd .. && anchor build            # produces target/deploy/soldust.so

# one scenario, on a fresh validator
./run.sh localnet/scenarios/c1-stranded-cursor.ts

# everything
./run-all.sh
```

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

`run-all.sh` runs that for you if the artifact is missing, and skips the scenario
rather than failing the suite if the build does not work. The shipped week/day
values are asserted by a `state.rs` unit test that only compiles when the feature
is off, so the short-fuse build can never be the one that passes review.

## The scenarios

| file | what it pins down |
| --- | --- |
| `00-smoke.ts` | baseline: push, seal, draw, settle. If this fails the harness is broken, not the program. |
| `c1-stranded-cursor.ts` | **C-1** an out-of-order refund used to move `settle_cursor` past a pending push and strand it, breaking the settle path for everyone behind it. The refund branch now respects the queue, so the batch drains from the head and every member gets out. |
| `c2-unreadable-vrf.ts` | **C-2** four ways an ORAO account can become unparseable, each of which used to make `expire_round` fail and close the escape hatch. All five rounds - a silent oracle is the control - now void and refund in full, because unreadable is not the same as landed. |
| `c3-stalled-star.ts` | **C-3** refunds get every stake out of a broken round, but they land no mass - so a star whose oracle never came back used to sit Alive forever holding a pot with no claimant, and no successor could be born past it. `collapse_stalled_star` now finishes any star that has gone a week without gaining mass (a day in the nursery) once its queue is empty: feeders take back the prize-side value of their own feeds and the settled pot endows the next star. Needs the `short-stalls` build, which `run-all.sh` produces. |
| `h1-frontrun-seed.ts` | **H-1** a squatted randomness address used to block a sealed round's draw forever. Seed and draw are one transaction now, so a squatter costs the crank one reverted attempt and the retry derives a different address. |
| `h2-initialize-race.ts` | **H-2** `initialize` had no signer gate, so the first caller owned the treasury permanently. It is now gated on the program's live upgrade authority, and the treasury it writes is unrevisable. |
| `h3-draw-race.ts` | **H-3** `round.entropy` used to be folded again on every arrival, so any push landing between a crank reading a round and its draw executing moved the ORAO address and reverted the draw - needing no attacker, only traffic, with the batch expiring into refunds if nobody won the gap. It is now committed once by the member who opened the round: the scenario derives the address before a second member joins and lands the draw on it afterwards. |
| `m1-rent.ts` | **M-1** the `Round` account's rent was charged to one arbitrary member and never returned. `close_round_account` now hands it back to `Round.opened_by`, so opening a round costs the same as joining one - and the scenario reads every rent figure the fee docs quote off a live validator. |
| `m3-dead-star-draw.ts` | **M-3** a finished star used to seal and draw a round whose members `resolve_push` was going to refund regardless. `draw_round` now requires a live star, so the float buys no draw nobody reads and those members exit immediately, rent included. |
| `m4-float-floor.ts` | **M-4** `withdraw_protocol_fees` is permissionless so the house need not be online to be paid, but with no floor a stranger could sweep the float `draw_round` reimburses cranks from, leaving third-party cranking unpaid. Collection now stops at `DRAW_FLOAT_FLOOR`, and only the treasury signing for itself can go below. |
| `regress-death-drain.ts` | regression for the C-1 fix: a supernova mid-queue still drains everyone behind it, in order, with the prize paid. Uses a ground draw to force the kill. |

## Note

Nothing here is deployed and nothing here is part of the program. The mock is its
own cargo workspace precisely so it cannot affect the audited build's lockfile or
release profile.
