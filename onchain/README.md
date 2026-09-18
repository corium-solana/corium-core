# SOLDUST - on-chain program (devnet MVP)

An Anchor program that runs the SOLDUST star lifecycle on Solana, plus a set of
TypeScript scripts for driving it by hand on devnet.

One global star exists at a time. Players push SOL at it. Every push that lands
grows the star and takes a verifiably-random roll at destroying it. Whoever
lands the lethal push is the **Star Killer** and takes the jackpot. The dead
star stays on-chain forever, and the next star is born from its death
randomness.

> **Status: devnet MVP.** It is written to handle player SOL correctly, but it
> has not been audited and there are deliberate simplifications. See
> [Before mainnet](#before-mainnet). Hosting (`www.corium.so`, DNS, the RPC
> proxy) is in [`docs/MAINNET.md`](../docs/MAINNET.md).

For a step-by-step walkthrough with copy-pasteable commands, go to
**[DEVNET_TESTING.md](./DEVNET_TESTING.md)**.

Player-facing rules (defaults taken from this program):
**[docs/RULES.md](../docs/RULES.md)**.

---

## Contents

- [Layout](#layout)
- [Accounts and PDAs](#accounts-and-pdas)
- [The push lifecycle](#the-push-lifecycle)
- [Randomness](#randomness)
- [Vault accounting](#vault-accounting)
- [Game configuration](#game-configuration)
- [Events](#events)
- [Build and deploy](#build-and-deploy)
- [Scripts](#scripts)
- [Security notes](#security-notes)
- [Before mainnet](#before-mainnet)

---

## Layout

```
onchain/
  programs/soldust/src/
    lib.rs              program entrypoint, instruction list
    game_config.rs      ** all game balance lives here **
    state.rs            Config / Star / Round / PendingPush / Player
    vrf.rs              MagicBlock VRF adapter, slot-hash read, seed + roll derivation
    vault.rs            the only place lamports leave the program
    events.rs           everything the frontend needs
    errors.rs
    math.rs             checked add/sub
    instructions/
      initialize.rs
      create_star.rs    create_first_star, create_next_star
      feed.rs           nursery hole ticket - one tx, no VRF
      request_push.rs   step 1 - the player's single signature; joins a round
      round.rs          step 2 - draw_round / consume_randomness / expire_round /
                        close_round_account
      resolve_push.rs   step 3 - permissionless settle/refund, plus close_push
      claim_prize.rs
      claim_hole.rs      pays feeders on a black hole *or* a stall collapse
      collapse_stalled.rs  the liveness floor: finish a star that went quiet
      admin.rs          withdraw_protocol_fees (permissionless above the float
                        floor), fund_protocol (permissionless)
  scripts/              TypeScript dev client
  localnet/             audit scenarios against a real validator + a mock oracle
  target/idl/soldust.json     generated IDL
  target/types/soldust.ts     generated TS types
```

After a build that changes accounts or instructions, `yarn sync-idl` copies the
IDL to `shared/chain/soldust.json` - the copy the live client, the dev scripts
and `Dockerfile` all read. It refuses to copy an IDL whose 32-byte const seeds
disagree with its own `address`, which is exactly how the pre-rename program id
survived in that file long after the rename.

---

## Accounts and PDAs

| Account | Seeds | Lifetime | Purpose |
|---|---|---|---|
| `Config` | `["config"]` | singleton | Current star id, the four vault liability buckets, and the frozen treasury. **No tunables, no authority, no pause flag** - every number that decides an outcome is compiled into `game_config.rs`. |
| vault | `["vault"]` | singleton | A **system-owned account with zero data** holding every lamport the program controls. Lamports can only leave via a `system_program::transfer` signed by the vault PDA. |
| `Star` | `["star", star_id_le]` | permanent | One per star, never overwritten. Carries the visual seed, mass, prize pool, stage, the round cursor, the settle cursor, and the full death record. |
| `Round` | `["round", star_id_le, round_id_le]` | per round | A batch of pushes that share one VRF draw. Carries the entropy its opening member committed, the sealed seed, the 32-byte draw itself once the oracle calls back, and the stage timestamps the expiry clock reads. |
| `PendingPush` | `["push", player, client_seed]` | per push | Binds a player, a stake, one star, and one round. |
| `StarFeed` / `FeedShare` | `["feed", star_id_le]` / `["feed-share", star_id_le, wallet]` | per star / per wallet | Nursery volume and one wallet's hole ticket. Created and paid for by the player, never by a resolver. |
| `Player` | `["player", wallet]` | per wallet | Lifetime STARDUST and counters. |

The push PDA is scoped to `(player, client_seed)` rather than to a counter, so
two people pushing in the same slot can never race for the same address. Reuse
after `close_push` is harmless: the address carries no authority, the `push_id`
is assigned from `star.push_counter` inside the handler, and the randomness a
push settles against belongs to its round, not to its address.

**A round's seed** is
`sha256("soldust:round" ‖ star_id_le ‖ round_id_le ‖ entropy ‖ slot_le ‖ slot_hash ‖ cranker)`,
where `entropy` is written once, by the push that opened the round:
`entropy = sha256("soldust:entropy" ‖ 0³² ‖ client_seed ‖ player ‖ push_id_le)`.

Every input is therefore either fixed when the round opened or chosen by the
caller of the draw. That was once a liveness requirement: the ORAO account
address followed from the seed and had to be named before the transaction
executed, so a field that moved as members arrived had any push confirming
mid-flight revert the draw - no attacker needed, just traffic. There is no
derived address to miss any more, so what the frozen field buys now is that the
seed is *predictable*: the crank derives it off-chain to tie a queued request
back to the round that made it, and clients replay the same derivation from
public data. A field that moved under them would break that tie silently rather
than loudly. `localnet/scenarios/h3-draw-race.ts` holds that down.

**A member's roll** is `sha256("soldust:roll" ‖ randomness ‖ push_id_le)[..16]`
read as a little-endian `u128`, mod `1e9`. One draw, one independent roll per
member, and `push_id` is fixed before the seed exists. `randomness` here is 64
bytes: the oracle delivers 32, and `vrf::widen` right-pads them so the roll is
computed over exactly the shape it always was.

All three derivations have known-answer vectors asserted on both sides -
`vrf::tests::round_seed_matches_the_typescript_client` in Rust, and the same
three constants in `scripts/selftest.ts`. The clients replay `roll_for` to show
a player their own outcome, so a drift here would silently break that proof.

`STARDUST` is an internal counter on `Player`. There is deliberately **no SPL
token**. Every settled push mints it: 2.5× in the nursery, then a declining
stage curve down to 0.8× at Event Horizon.

`Star.current_round` is the round `request_push` stamps onto new pushes;
`Star.settle_cursor` is the next `push_id` allowed to resolve. The cursor counts
refunds and nursery feeds as well as live settles, because ids are dense and
every push is processed exactly once - which makes it an exact ordering guard
that a voided batch drains through rather than wedging.

`Star.last_mass_ts` is when the star last *gained* mass: birth, a feed, or a
settled push. It is the stall clock, and the distinction is the point - refunds,
closed rounds and voided rounds all move a star without anyone winning anything,
so a star that can only do those is stalled. See `collapse_stalled_star` under
[Nothing can freeze](#nothing-can-freeze).

---

## The push lifecycle

```
                    create_first_star / create_next_star
                                  |
                  mass < 1 SOL    |    mass >= 1 SOL
                       |          |         |
  player signs ->   feed     request_push ------------->  PendingPush{Pending}
                    one tx    escrow stake only              joins Round{Open}
                    hole      pending_liability += amount     entropy += seed
                    ticket    no oracle prepayment            member_count += 1
                                  |
                                  |   draw_round (anyone, after the window or at
                                  |   target size). Seals the round, derives the
                                  |   seed from a caller-named slot hash, and
                                  |   files one VRF request for the whole round
                                  |   out of protocol_accrued - one transaction,
                                  |   so the seed is never public while unspent.
                                  v                        Round{Requested}
                                  |
                                  |   consume_randomness. An oracle answers and
                                  |   the VRF program calls back into us with the
                                  |   32-byte draw, which lands on the round.
                                  |   Nobody else can produce that signature.
                                  v                        Round{Drawn}
  anyone      ->   resolve_push  (permissionless, settle_cursor order)
                                  |
        star alive & drawn -------+------- star dead, or Round{Expired}
                |                                    |
   settle, STARDUST, own roll                 PushStatus::Cancelled
      survived / killed / hole @ 21           full refund in the same tx
                                    |
                          claim_prize (killer) / create_next_star (anyone;
                          also once the star has committed 21 SOL)

  stalled at either stage for ROUND_EXPIRY_SLOTS -> expire_round (anyone)
  -> Round{Expired} -> every member refunds. Refuses if the draw landed.

  every member resolved -> close_round_account (anyone) -> rent to opened_by

  no new mass for STALL_SECS, nothing queued -> collapse_stalled_star (anyone)
  -> Star{Stalled} -> feeders refund at cost via claim_hole_share, the pot the
  settled pushes built endows the next star. The liveness floor: needs no
  oracle, no crank of ours, and no upgrade.
```

There is deliberately **no state between `Open` and `Requested`.** A round that is
sealed but not yet drawn was a real status once, and it was a vulnerability: its
seed was public while the ORAO account that seed pointed at was still unoccupied,
and ORAO's request instruction was permissionless. `RoundStatus` no longer has a
variant to represent that moment, because the moment no longer exists. The
mechanism behind the finding is gone too - MagicBlock files a request against a
queue instead of creating an account at an address the seed decides, so there is
nothing to occupy. The atomicity is kept anyway, because it costs nothing and it
preserves the cleaner property: a seed is never public while still unspent.

**Only the first transaction is signed by the player.** Resolution is
permissionless: a backend crank normally submits it, but the crank has zero
influence on the outcome - everything it needs is read from accounts it cannot
forge, and anyone can submit the same transaction with the same result.

Concurrency works the way you would expect. Alice, Bob and Carol can all have
pending pushes on the same star. Whoever's resolution lands first is processed
first. If Carol's is lethal, the star dies, and Alice's later resolution takes
the dead path and is refunded automatically. This is intentional.

A feed is one transaction: take leftover nursery room, write the hole ticket,
done. No VRF. If Alice's 1 SOL lands after Bob already took 0.5, she is
clipped to 0.5 in that same tx. If the nursery is already full, `feed` fails
and no SOL is taken. A signed feed never becomes a last-hit.

Last-hit `request_push` escrows the stake, joins the star's open round, folds the
player's `client_seed` into that round's entropy, and stops. It opens no oracle
account and prepays nothing. If the queue has moved on and the live max is now
below what was quoted, the excess is refunded at settle time and the rest lands.

That is what makes a wasted push free. A push that never rolls costs the player
the network fee and nothing else, whether the star died first or the round was
voided. The 3.14% rake is unaffected either way.

A **dead-star resolve does not require randomness at all**, and neither does a
voided round. Between them those two paths mean escrow is never stuck: if the
oracle stops answering, `expire_round` releases the batch after the timeout, and
if the star dies first the refund path takes over immediately.

---

## Randomness

[MagicBlock `ephemeral-vrf`](https://github.com/magicblock-labs/ephemeral-vrf)
(`Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz`, same address on devnet and
mainnet), against the oracle queue
`Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh`, which the program pins. No
timestamp or blockhash decides anything. A recent slot hash is read, but only as
a seed **nonce** - never as randomness.

The integration is **push, not pull**. `draw_round` files a request against the
queue; an oracle answers it off-chain and the VRF program calls back into
`consume_randomness`, which writes the draw onto the round. There is no
per-request account, no address derived from the seed, and nothing of the
oracle's for this program to parse.

### One draw per round, not per push

A draw costs a flat 0.0005 SOL, and that is the whole of it - there is no
per-request account, so no rent is locked up and none comes back. Charged per
push that is still 5% of a minimum 0.01 SOL push, which would make small play
expensive for no reason. So randomness is bought per **round**: every push that
arrives inside the same short window shares one draw, and each member derives an
independent roll from it.

| Members sharing one draw | Cost per push |
|---|---|
| 1 | 0.0005 SOL |
| 4 | 0.000125 SOL |
| 10 | 0.00005 SOL |
| 24 | ~0.0000208 SOL |

At 314 bps a round has paid for its own draw once it holds about 0.016 SOL of
stake - two minimum pushes, or one member of any real size. Below that the house
subsidises the batch, which is the right direction for the error to run.
`vrf::tests::a_round_amortises_the_draw_across_its_members` pins these.

Under ORAO the same bar sat at about 0.07 SOL, or seven minimum pushes, because
ORAO entombed ~0.00168 SOL of unrecoverable rent in every draw on top of its fee.
That is the practical difference the migration bought: a round of two minimum
pushes is now worth drawing, so quiet stars stop waiting for company that was
only ever needed to pay for rent.

### Why the seed cannot be gamed

Two separate properties are load-bearing here, and it is worth keeping them apart
because they fail in different ways:

1. **A round's seed must not be computable by anyone until the round is closed to
   new entrants** - otherwise a member could decide whether to join.
2. **A draw must only ever reach the round that asked for it, and only from the
   VRF program** - otherwise randomness this program did not buy could be read as
   a round's outcome.

For (1), three ingredients cover each other's weakness:

- Each member's `client_seed` is folded into the round's entropy as they join, so
  a block leader ordering transactions does not have sole control of the seed.
- A recent slot hash is mixed in, so a member who dislikes the entropy
  accumulated so far cannot re-roll it by pushing again - they cannot predict
  which hash the crank will name.
- The **cranker's own address** is mixed in, so the seed is not a function of
  public state alone. A crank that signs each draw with a fresh keypair produces
  a seed nobody else can compute even in principle.

For (2), two facts are fixed in different places and both have to hold.

The **caller is the VRF program**. `consume_randomness` pins its one signer to
`vrf::callback_identity()`, which is `PDA(["identity", soldust], vrf)` - a PDA
derived under the VRF program's own id and scoped to this program. Only that
program can produce a signature for it, via `invoke_signed`, and it only does so
after verifying an oracle's proof against a request in its queue. The key cannot
appear as a transaction signer at all, so there is no way to present it from
outside. Anything else is `InvalidVrfCallbackIdentity`.

The **round is the one that asked**. `draw_round` names the round in the
request's callback account list, and nothing later can change that entry, so the
oracle can only hand a result back to the round that bought it. The request
itself is signed by `PDA(["identity"], soldust)`, and MagicBlock refuses a
request whose identity does not derive from the callback program it names - so
an outsider cannot file a request that calls back into soldust in the first
place.

Two more checks make the write total. `consume_randomness` requires status
`Requested` (`RoundNotRequested`), so a draw is written once: a replayed callback
finds the wrong status, and a round that already expired into refunds cannot be
revived into a settlement. And an all-zero draw is refused (`ZeroRandomness`),
because all-zero is the byte pattern an undrawn round already carries and storing
it would make "drawn" and "no draw yet" indistinguishable to every reader
off-chain.

Property (2) used to read *"a round's seed must never exist on chain without its
ORAO account"*, and that was a real finding rather than a hypothetical. Sealing
and drawing were two transactions: `close_round` published `round.seed` while the
ORAO account derived from it was still empty, and ORAO's `request_v2` was
permissionless, so anyone reading the sealed round could occupy that exact
address first. `request_round_vrf` correctly refused to adopt an account it had
not created - which meant the round could then only ever expire. It cost the
attacker one ORAO fee to deny a round, repeatably. The fix was not a check but
the removal of the gap: sealing and drawing merged into `draw_round`, so
`RoundStatus` has no sealed-but-undrawn variant.

Under MagicBlock the underlying hazard is gone rather than merely closed. There
is no address derived from the seed, so there is nothing to squat, and filing a
request at all needs a signature only this program can produce. Sealing and
drawing are still **one transaction** because that costs nothing and keeps the
seed from ever being public while unspent.
`localnet/scenarios/h1-frontrun-seed.ts` now pins the gate that supersedes the
squat: a stranger trying, three ways, to file a request naming soldust as its
callback.

`draw_round` takes the `seed_slot` from its caller rather than sampling one
itself. Under ORAO it had to, because Solana requires every account address up
front and the randomness address was a function of the seed. That constraint is
gone, and the parameter is kept because it is what makes the derivation
replayable off-chain from public data alone, which the crank needs to tie a
queued request back to its round and the clients need to replay a roll.
`vrf::slot_hash_at` verifies the named slot really is in `SlotHashes` and within
`SLOT_HASH_LOOKBACK` (150 slots, matched to a blockhash's own lifetime so the
check can never be the reason an otherwise-valid transaction fails). That
discretion is harmless: a seed cannot be evaluated without the VRF output, so
there is nothing to shop for.

The oracle delivers 32 bytes where ORAO delivered 64. `Round.randomness` used to
hold the ORAO account's *address* - a `Pubkey`, which is 32 raw Borsh bytes -
and now holds the draw itself in the same 32 bytes, so the account layout did not
move and live rounds needed no migration. `vrf::widen` right-pads the draw to 64
before rolling, so `roll_for` and the published test vectors are bit-for-bit what
they were. The oracle contributes 256 bits of entropy instead of 512; every
property the game depends on comes from SHA-256 acting as a PRF over the
`push_id` label rather than from how wide its input is.

### Nothing can freeze

`ROUND_EXPIRY_SLOTS` (750, ~5 min) is measured from whichever stage a round
stalled in, so a stage that made progress is not punished for it. Past that,
anyone can call `expire_round`; members then refund through `resolve_push` in
`push_id` order like any other resolution, no randomness required, and the star
continues with a fresh round.

Two properties keep that from being an exploit rather than an escape hatch:
`expire_round` refuses when the draw has already landed, so it can never discard
an unfavourable roll; and while a draw is pending nobody knows its value, so
voiding is always blind.

The first of those used to require parsing ORAO's account and deciding what an
unreadable answer meant - a defensive branch that had to void the round, because
refusing to would have closed the escape hatch exactly when the oracle was the
broken thing. Under the push model it is a status comparison: a `Drawn` round
reports no stall slot at all, so a landed draw is structurally unvoidable, and
`expire_round` takes no oracle account. Neither does `resolve_push`, because the
draw is already on the round.

Every stake therefore has an oracle-independent exit, and
`onchain/localnet/scenarios/c2-unreadable-vrf.ts` demonstrates it against a
forged callback, a replayed one, an all-zero draw, a callback arriving after the
round already voided, and a silent oracle.

**And the pot, one step behind it.** Refunds alone were not enough, because a
refund lands no mass. If the oracle stopped answering permanently, rounds would
keep expiring and keep refunding, but no push would ever roll again - so the star
would never die, never reach the hole, and the `prize_pool` its earlier settles had
already paid in would sit in the vault with no instruction able to release it. Not
a loss of principal, but the feeders' hole tickets would be worthless paper and no
successor could be born past the incumbent. The identical shape appears with
nothing broken at all, if people simply stop playing.

`collapse_stalled_star` is the floor under that. Once a star has gone
`STALL_SECS` (7 days) without gaining mass - `NURSERY_STALL_SECS` (24 hours) if it
never left its nursery - and its queue is empty, anyone may finish it. Feeders
then claim through the ordinary `claim_hole_share` path, and the pot is split by
one rule:

> feeders get back the prize-side value of exactly what they fed; the remainder
> recycles into `next_star_reserve` and endows the next star.

That rule is what makes a permissionless collapse safe, and it is worth being
precise about why, because an earlier revision of this document argued the
opposite - that any such hatch would be a lever deciding a game's outcome without
a roll, pointable at a live star by whoever could satisfy its conditions, with a
sole nursery feeder arranging to be the only claimant.

It is not a lever, because **it never pays anybody a profit**:

- A feeder is paid `prize_bps` of their *own* feeds, the same 314 bps rake every
  push pays, and in exchange gives up the 21:1 hole ticket they were holding. A
  collapse is strictly worse for them than the star continuing, so a sole nursery
  feeder gains nothing by arranging one - the "only claimant" case pays them 96.86%
  of their own money.
- The house is paid nothing. The residue is prize money, `protocol_accrued` is not
  touched, and a prize bucket is not withdrawable - it can only ever fund the next
  star.
- Nothing leaves the vault at all. The instruction moves lamports between two
  liability buckets and sets a status; the transfers happen later, one feeder at a
  time, through the same claim path a black hole uses.

And it cannot cut in front of live play: `pending_pushes` must be zero, so no push
that still has a roll coming can be cancelled by it, and the timer restarts on
every settle and every feed, so a star has to be genuinely idle for a week. The
worst a hostile caller can do with it is end a game nobody was playing, one week
late, by paying its fee. Pinned by
`onchain/localnet/scenarios/c3-stalled-star.ts`.

What that buys is the whole liveness argument, and it holds with the oracle dark
and every player gone: voiding a round needs only slots, resolving its members
needs only that void, and collapsing the star needs only time. None of the three
needs us, and none needs an upgrade authority.

The SlotHashes sysvar is parsed by hand in `vrf::slot_hash_at` - 8-byte LE
count, then 40-byte `(slot, hash)` entries, newest first. Anchor's
`Sysvar<'info, SlotHashes>` is deliberately avoided: the account is ~20KB and
deserialising it would cost more compute than the rest of the instruction.

### Why the CPI is hand-rolled

The `ephemeral-vrf-sdk` crate pulls in a `solana-program` / `steel` stack of its
own, and this program deliberately depends on nothing but `anchor-lang` and a
SHA-256 hasher so its build stays reproducible. The same reasoning kept the ORAO
adapter hand-rolled before it, where the conflict was a pinned
`anchor-lang ^0.32.1` against the Anchor 1.x this program targets.

The surface is tiny - one instruction to build and one 32-byte argument to
receive - and every constant in `vrf.rs` is asserted against its published
derivation in that module's unit tests: the `RequestRandomnessScoped` tag, our
own `consume_randomness` discriminator, the request fee, and both identity PDAs
against the deployed program ids. The request is the *scoped* form rather than
the legacy global one on purpose: a scoped request is answered by a callback
signed by a PDA bound to this program id, where the deprecated global identity
would let any program's callback be signed by the same key ours is checked
against.

### Cost, on top of the stake

Nursery `feed` has no VRF and none of this. A last hit signs the stake plus rent
for the accounts it has to open. These are `(128 + bytes) × 5080` lamports at the
current rate - mainnet and devnet both quote it - and
`localnet/scenarios/m1-rent.ts` reads every one of them back off a live validator:

| | Rent | Comes back? |
|---|---|---|
| `PendingPush` | 0.0014224 SOL | Yes, `close_push` returns it to the player |
| `Round` | 0.0017577 SOL | Yes, `close_round_account` returns it to `opened_by`, once every member resolves |
| `Player` | 0.0011430 SOL | No - once per wallet, ever |
| `FeedShare` | 0.0009449 SOL | No - once per wallet per star |
| `StarFeed` | 0.0007772 SOL | No - once per star, paid by whoever gets there first |

Only the batch's first member pays the `Round` rent, and a wallet cannot know in
advance whether that will be it, so an interface has to quote it either way. Worst
case - new wallet, new star, and it opens the round - is 0.0060452 SOL signed on
top of the stake, of which 0.0031801 comes back. Every push after that on the same
star signs a push rent that comes straight back, and the network fee.

That is the whole bill. **The player pays nothing towards randomness.** The
frontend's copy of these figures is `src/legal/fees.js`; if an account's layout
changes, that scenario prints the new number and both documents follow it.

**What the house pays** is one flat fee, the same on devnet and mainnet:

| | devnet | mainnet |
|---|---|---|
| `VRF_REQUEST_FEE`, paid into the queue | 0.0005 | 0.0005 |
| per-request rent | none | none |
| **unrecoverable cost of one draw** | **0.0005** | **0.0005** |

There is no rent term because there is no per-request account. That is the whole
of the economic change from ORAO, which charged 0.0005 SOL in fees on mainnet
*and* entombed 0.00168 SOL of rent in every fulfilled randomness account, for
0.00218 SOL unrecoverable per draw - about 4.4× what a draw costs now.

The figure is a compile-time constant rather than something read off chain, which
is a real difference from the ORAO adapter: ORAO published its fee in a
`NetworkState` account so the price could be read live, while MagicBlock's is a
`const` in their program with no account to read it from. The exposure that
creates is bounded on purpose and lands on the house either way. The *gate* in
`draw_round` prices a draw with this constant, so a reprice upwards would let
through a round that no longer quite covers its own draw - a shortfall capped at
the difference and paid out of the rake, never out of stake. The *reimbursement*
never uses the constant at all.

How the crank is made whole:

* `draw_round` measures the cranker's balance across the VRF CPI, so the
  outlay is whatever MagicBlock actually charged, not an estimate. Nothing is
  held back and nothing comes back, so the whole outlay is the cost.
* It reimburses `cost.min(config.protocol_accrued)` out of the house cut. The
  `min` matters: the instruction can therefore never fail for lack of funds, so a
  starved till slows nothing down and cannot stall the game. A third-party crank
  running against an empty till absorbs the difference.
* `fund_protocol` seeds that float once at genesis, because the rake cannot accrue
  until a push settles and a push cannot settle until a draw is paid for.

The player is not in that loop at all. A push that never rolls costs them the
network fee; a draw that reprices mid-round costs the house, not them.

---

## Vault accounting

Every lamport in the vault belongs to exactly one of four buckets tracked on
`Config`, plus the account's own rent-exempt minimum:

| Bucket | Meaning |
|---|---|
| `pending_liability` | Escrowed push stakes not yet settled or refunded. **Not part of any jackpot.** |
| `prize_liability` | Prizes owed to star killers and hole feeders, claimed or not. |
| `next_star_reserve` | Prize from a collapse nobody fed, waiting to endow the next star. Not withdrawable. |
| `protocol_accrued` | Protocol revenue, **and** the float draws are bought from. The only bucket `withdraw_protocol_fees` can move. |

`withdraw_protocol_fees` applies two independent guards: the amount must be
within `protocol_accrued`, **and** the vault balance minus the other three
buckets and `rent_minimum` must still cover it. A bug in one bucket cannot leak
player funds through the other.

A third guard is about liveness rather than solvency. Because that bucket is also
the draw float, a permissionless withdrawal may only take what is above
`DRAW_FLOAT_FLOOR` (0.05 SOL, a hundred draws); going below it needs
the treasury's own signature. Collecting revenue therefore never requires the
house to be online, but a stranger cannot leave third-party cranks paying for
randomness out of pocket. Pinned by `localnet/scenarios/m4-float-floor.ts`.

`yarn show-config` prints all of this along with a solvency check.

---

## Game configuration

**Everything lives in [`programs/soldust/src/game_config.rs`](./programs/soldust/src/game_config.rs)
and is compiled into the program.** There is no `update_config`. Changing a
number requires a program upgrade. Mainnet's upgrade authority is the Squads
multisig `9BfEudxsWmyPP6uGHRyyMHptShK6DVufZSkYd8aaHAdx`, so the program is
upgradeable today; the intent is still to set it to `None` once the game has run
long enough to trust the code without a rescue hatch.

`Config` stores **no** copy of these numbers. It used to, as a courtesy for
explorers, and that was removed: a snapshot on an account protects nothing,
because an upgrade that wanted to change the odds could equally well ignore the
snapshot. The honest statement is that the binary is the rulebook and the
upgrade authority is the only thing above it. Handlers call
`game_config::economics()` / `lifecycle()` directly.

A live star still copies the compiled lifecycle at birth, but that only freezes
its STARDUST taper now. The odds are not a table an upgrade could retune:

```
nova chance = amount / (mass_before + amount)
```

Your stake against the pot you are shooting at, as straight odds. There is no
minimum that tracks the jackpot, no per-stage rate, no exposure unit, no
compounding and no cap. Three things fall out of that and none of them were
tuned:

**It pays exactly `prize_bps`.** A kill takes `prize_pool` measured after the
push lands, and `prize_pool` is exactly `prize_bps` of mass, so
`EV = [a/(M+a)] × 0.9686(M+a) = 0.9686a`. The `(M+a)` cancels: every bid size, at
every pot size, at every stage, returns 96.86%. The 3.14% rake is the only edge.

That "exactly" is load-bearing and it is why the rake is 314 bps rather than any
neighbouring number. `Economics::split` works in integer lamports, so if
`protocol_bps` of one `PUSH_STEP` were not a whole number the split would round
and `prize_pool` would stop being precisely `prize_bps` of mass. 314 bps of
0.01 SOL is exactly 314 000 lamports, so π survives contact with the lattice and
the identity holds to the lamport rather than approximately.

**Survival telescopes.** One push survives with probability `M/(M+a)`, so a run
of them survives with `M0/M1 × M1/M2 × … = M0/Mn`. Star mass is a martingale, so
a star reaches the hole exactly `FEED_MASS / HOLE_MASS` = **1/21** of the time
whatever anyone pushed, and nursery feeders are taking fair 21:1 odds on that -
which returns them the same 96.86%. Feeders and pushers sit on one book by
construction rather than by tuning.

**The queue cannot hurt you.** The threshold is computed at settle from settled
mass. If someone lands ahead of you your chance falls and the pot you are
shooting at rises by the same factor, so your EV does not move. Nothing a signed
amount depends on can go stale, which is why there is no longer any minimum to
miss and no `PushAmountOutOfRange` waiting between your wallet and the chain.

The only shape rule left is `PUSH_STEP`: whole multiples of **0.01 SOL**,
clipped to the room left to the nursery cap (1 SOL) or the hole cap (21 SOL).
Both caps are whole numbers of steps, so mass stays on the lattice and the room
left to either is always itself a legal push. The largest chance available at
mass `M` is therefore `(21−M)/21`: a one-shot is cheap on a young star (95.2% at
1 SOL) and near-impossible next to the hole (4.8% at 20 SOL).

Stages are cosmetic and economic flavour only - mass boundaries for the visuals
and a STARDUST multiplier:

| Stage | from | dust |
|---|---|---|
| PROTOSTAR | 0 SOL | 2.5× |
| MAIN SEQUENCE | 2.0 | 1.5× |
| BLUE GIANT | 6.0 | 1.3× |
| RED GIANT | 10.0 | 1.15× |
| SUPERGIANT | 14.0 | 1.0× |
| CRITICAL | 18.0 | 0.9× |
| EVENT HORIZON | 21.0 | 0.8× |

Economics are a 96.86 / 3.14 prize / protocol split, pushes in whole 0.01 SOL
steps up to the room left, and 1 000 STARDUST per SOL × the stage curve × a
small bonus for the chance you bought (1% → 1.00, 10%+ → 1.20).

Probabilities are parts-per-**billion**, not millions: the smallest legal push
against the largest legal pot is 0.01/21, about 476 190 ppb, and billionths keep
even that exact to six figures while still fitting a `u32`.

`cargo test` asserts the identities above directly rather than checking a curve
against reference numbers - that every size returns exactly `prize_bps`, that
survival telescopes, that the hole rate is `FEED_MASS/HOLE_MASS`, that the
feeder and pusher books close, and that the lattice is closed under pushes. It
also asserts the shape invariants that used to be a runtime `validate()` -
stages ascending from zero, splits summing to 10 000, both mass boundaries on
the step lattice. Those are compile-time constants, so a passing build is the
guarantee; there is nothing left to check on chain.

Stage *names* are off-chain only (`STAGE_NAMES` in `scripts/lib.ts`).
`Star.stage` is an index into the configurable table, and `Star.status` carries
Alive/Dead separately.

---

## Events

The frontend should drive off these rather than polling accounts.

| Event | Frontend equivalent |
|---|---|
| `StarCreated` | StarCreated |
| `PushRequested` | PushPending (carries `round_id` and how many are sharing the draw) |
| `RoundClosed` | the round sealed; carries the seed and the slot whose hash went into it |
| `RoundRequested` | the draw was bought; carries `cost` and `cost_per_member` |
| `RoundDrawn` | the oracle called back; carries the 32-byte draw, and is the signal a crank waits on before resolving the round's members |
| `RoundExpired` | the round was voided; every member is now refundable |
| `PushResolved` (`survived: true`) | PushSurvived |
| `PushResolved` (`survived: false`) | lethal push, paired with `StarDestroyed` |
| `StageChanged` | StageChanged |
| `StarDestroyed` | StarDestroyed |
| `PushCancelled` | PushRefunded (`round_voided` says which kind of refund it was) |
| `PrizeClaimed`, `HoleShareClaimed`, `StarCollapsed`, `ProtocolFunded`, `ProtocolFeesWithdrawn` | - |

`StarDestroyed` carries the full 64-byte VRF output - the oracle's 32 bytes as
widened by `vrf::widen` - along with the roll and the threshold, so anyone can
verify a death independently. `yarn watch` tails the whole stream.

The program exposes facts only - masses, seeds, stages, rolls. It knows nothing
about the visuals.

---

## Build and deploy

Requires the Solana CLI (Agave 4.x), Rust, and Anchor 1.2.0.

```bash
cd onchain
yarn install
anchor build            # program + IDL + TS types
cargo test              # 50 pure-logic unit tests
yarn selftest           # offline IDL/encoding/seed/layout check, no network
./localnet/run-all.sh   # the audit scenarios, on a throwaway validator each
```

`localnet/` needs its mock oracle built once first - see
[localnet/README.md](./localnet/README.md).

Deploy and drive it: **[DEVNET_TESTING.md](./DEVNET_TESTING.md)**.

> `program-keypair.json` is the program's **identity** (the program id), not the
> upgrade authority. It is gitignored. Keep it off GitHub. After `solana-keygen
> grind`, copy the hit to `program-keypair.json` and
> `target/deploy/soldust-keypair.json`, then `anchor keys sync && anchor build`.

### Upgrading mainnet

Mainnet upgrade authority is a **Squads vault**, so `solana program deploy`
cannot run from a workstation - the loader wants the authority's signature.
And `ProgramData` was allocated at exactly `45 + len`, so *any* growth in the
binary needs `extend` first or the upgrade fails on account size.

This MagicBlock binary is 3,160 bytes over the currently allocated ProgramData
(524,248 byte `.so` + 45-byte header vs 521,133 allocated). `extend` is
permissionless; 32,768 extra bytes costs ~0.167 SOL of rent and leaves room
for a later patch.

Cutover order, because `draw_round`'s accounts changed and the crank talks to
them:

1. Drain. `yarn show-pending` must be empty. A `Requested` round bought from
   ORAO cannot receive a MagicBlock callback, and `resolve_push` now refuses
   anything that is not `Drawn`, so those members would wait out
   `ROUND_EXPIRY_SLOTS` for a refund. (Mainnet is already empty as of the
   2026-09-17 verified build.)
2. Pause the production crank (`fly machine stop` on `soldust-crank`'s `app`
   process, or scale it to 0) so it cannot file an ORAO draw into the gap.
3. `yarn verify-build sizes` then `yarn verify-build extend 32768`.
4. `yarn verify-build buffer` — write-buffer, then hand the buffer to
   `9BfEudxsWmyPP6uGHRyyMHptShK6DVufZSkYd8aaHAdx`.
5. Squads → Developers → Program upgrade, program
   `CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S`, that buffer. Confirm the
   on-chain hash is `547fd72365cfbb358a21fbbcfb85006571fa1e4950f8ee524f1e130ed8f0d402`
   (or whatever the Action produced, if it differs — deploy the Action artifact).
6. Immediately `fly deploy -c fly.prod.toml` so the crank's `draw_round` metas
   match. MagicBlock is already on mainnet at
   `Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz`, queue
   `Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh`.
7. Land one round. `yarn tsx scripts/audit-vrf.ts --round <pda>` against it.
   Then, if you want the explorer badge, commit, `yarn mirror-public`, and
   `yarn verify-build pda-tx` through Squads.

```bash
cd onchain
yarn verify-build sizes            # binary vs allocated, tells you the deficit
yarn verify-build extend 32768     # permissionless, payer is your wallet
yarn verify-build buffer           # write-buffer, then hand it to the vault
```

Buffer rent comes back to the spill account when the upgrade executes.

### Verified builds

Deploys come from the Docker `.so` that
[`solana-verify`](https://github.com/solana-foundation/solana-verifiable-build)
produces, not from `anchor build`: only the pinned image is reproducible, and the
explorer compares against exactly it.

The two clusters are not on the same binary while the oracle migration lands:

| Cluster | Deployed hash | What it is |
|---|---|---|
| mainnet | `577d0e0954ed7402aee490f0f6d15af2701743822b5dae08fda76d9e433632a9` | the verified ORAO build |
| devnet | `e9b07c4998790ab76263e8bdea17db7bb80279eb436ee7801a226d0ef555d20d` | a local, **non-verified** MagicBlock build |
| both, next | `547fd72365cfbb358a21fbbcfb85006571fa1e4950f8ee524f1e130ed8f0d402` | MagicBlock, Docker image Solana 4.0.3 / SBPF v3. Produced locally via amd64-on-arm64; confirm with the `verifiable-build` Action before quoting it as verified. |

`yarn verify-build` hard-fails on `overflows the maximum allowed frame space`. This artifact passed that gate. ProgramData on mainnet is 3,160 bytes too small for it (`sizes` reports the deficit); extend before writing the buffer.

```bash
cargo install solana-verify --locked
cd onchain
yarn verify-build                  # docker .so -> target/deploy/soldust.so
yarn verify-build hash             # local vs on-chain
```

Two things that are easy to get wrong. `solana-verify` defaults to `--arch v0`
while this program is **SBPF v3** (ELF `e_flags 0x03`), so every command here
passes `--arch v3`; it is recorded in the verify PDA so OtterSec reproduces the
same thing. And the image is stricter about stack frames than the local 4.2.x
toolchain - it is what caught `try_accounts` for `RequestPush`/`ResolvePush`
running ~500 bytes past the 4096-byte limit, which broke every `request_push`
with `ConstraintSeeds` until both were boxed. `yarn verify-build` hard-fails on
`overflows the maximum allowed frame space` rather than handing back that
artifact; if it ever trips, do not deploy, and put the binary through
`localnet/run-all.sh` (`validator.sh` honours `SOLDUST_SO`).

The *Verified* badge additionally needs a public repo OtterSec can clone and a
PDA signed by the program authority - the Squads vault, so it cannot be signed
here:

```bash
yarn mirror-public                 # source-only public repo, fresh history
VERIFY_REPO=https://github.com/<org>/<public-repo> yarn verify-build pda-tx
# sign in Squads, then:
solana-verify remote submit-job --program-id <id> --uploader <vault>
```

Before claiming the badge, run the `verifiable-build` Action and check its hash
equals the deployed one. An Apple-silicon docker build usually reproduces the
x86 image but is not guaranteed to; if the Action disagrees, its artifact is the
one to deploy.

---

## Scripts

All of them accept `--wallet <path>` (default `ANCHOR_WALLET` or
`~/.config/solana/id.json`) and `--url <rpc>` (default `SOLANA_RPC_URL` or
public devnet). Copy `.env.example` to `.env` to set defaults.

| Command | What it does |
|---|---|
| `yarn initialize` | Create `Config` + vault, then star #1. |
| `yarn show-config` | Accounting, vault solvency, and the compiled curve. |
| `yarn show-current-star` | Star state plus the live next-push nova chance. `--star N` for an old one. |
| `yarn push` | Escrow a stake and join the open round. `--amount`, `--count`. |
| `yarn resolve` | Advance rounds, then settle everything ready. `--push <pubkey>`, `--close`, `--no-rounds`. |
| `yarn crank` | Event-driven seal / draw / resolve. `--interval` must stay under the round window (default 4s). `--auto-next-star`. |
| `yarn show-pending` | Pushes and where their round has got to. `--all`, `--star`, `--player`. |
| `yarn show-player` | Wallet stats. `--player`, `--leaderboard`. |
| `yarn create-next-star` | Birth the successor to a dead star. Permissionless. |
| `yarn claim-prize --star N` | Collect a jackpot. Must be the killer. |
| `yarn withdraw-fees` | Move protocol revenue to the treasury. Permissionless. |
| `yarn watch` | Tail decoded events. |
| `yarn selftest` | Offline layout / encoding / seed check. No network. |

There is deliberately no script to change a game number, because there is no
instruction to change one.

---

## Security notes

What the program does about each thing worth worrying about:

| Concern | Handling |
|---|---|
| PDA seeds / account substitution | Every account is either a `seeds`-constrained PDA or pinned with `address =`. `Star` is bound to `pending_push.star_id`, so randomness can never resolve against a different star. |
| Signer validation | `claim_prize` compares the signer to `star.killer`. `withdraw_protocol_fees` is permissionless but can only pay the treasury frozen at initialize, and only the treasury signing for itself can take the balance below the draw float floor. |
| Duplicate push resolution | `resolve_push` requires `PushStatus::Pending` and sets a terminal status in the same transaction. |
| Settling against known randomness | Impossible by construction now that randomness belongs to the round rather than to the push address. A round's seed does not exist until `draw_round` seals it, the round stops accepting members in that same transaction, and a member's `push_id` was fixed before then. Reusing a push address after `close_push` therefore gains nothing. |
| Grinding the round seed | The `client_seed` committed at open is only one input; `draw_round` also mixes in a slot hash the crank names and the crank's own address, neither of which any member can predict, and neither of which exists when they sign. Pushing again cannot re-roll a round - a later push does not touch the seed at all. |
| Buying a draw for a soldust round from outside | There is no address a seed points at to squat any more, and a request that names `consume_randomness` as its callback has to be signed by `PDA(["identity"], soldust)` - the VRF program rejects an identity that does not derive from the callback program it names. Sealing and requesting stay one transaction regardless, so a seed is never public while unspent. Reproduced in `localnet/scenarios/h1-frontrun-seed.ts`. |
| Naming a slot hash to steer the seed | `vrf::slot_hash_at` requires the slot to be present in `SlotHashes` within `SLOT_HASH_LOOKBACK`, but the discretion is not worth constraining harder: a seed cannot be evaluated without the VRF output, so there is no favourable one to choose. |
| Voiding a round to dodge a bad roll | A `Drawn` round reports no stall slot at all, so `expire_round` refuses it outright - no oracle account is read, and there is no foreign layout to get wrong. Before a draw lands nobody knows its value, so the choice to void is always blind. |
| A stalled round freezing stake forever | `expire_round` is permissionless after `ROUND_EXPIRY_SLOTS`, measured from whichever stage stalled. Members then refund with no randomness needed. |
| A voided round wedging the queue | `star.settle_cursor` counts refunds and nursery feeds as well as live settles, so a voided batch drains through the ordering guard instead of blocking every push behind it. |
| A dead oracle stranding a star's pot forever | `collapse_stalled_star` is permissionless once a star has gone `STALL_SECS` without gaining mass with nothing queued. It pays feeders the prize-side value of their own feeds and recycles the rest into the next star, so it can never be farmed: no caller profits, the house takes nothing, and no lamport leaves the vault. `localnet/scenarios/c3-stalled-star.ts`. |
| Collapsing a star that is still in play | The stall clock is reset by every settle and every feed, `pending_pushes` must be zero, and the window is a week (a day in the nursery). So a collapse cannot cancel a push that has a roll coming, and cannot reach a star anybody is playing. |
| A refund stranding a star below the hole cap | An expired round can hand back enough stake to drop committed mass under `HOLE_MASS` *after* the next star already exists - leaving a star that can never take mass, nova, or reach the cap. `resolve_push` detects that (`stranded()`) and collapses it into a hole, releasing the pot rather than locking it in `prize_liability`. |
| Duplicate prize claims | `prize_claimed` is latched **before** the transfer; the whole thing reverts together on failure, so it is retryable but never double-payable. |
| Integer overflow | `overflow-checks = true` in release, plus explicit `checked_add`/`checked_sub` returning `MathOverflow`. |
| VRF validation | `consume_randomness` pins its one signer to `vrf::callback_identity()` - `PDA(["identity", soldust], vrf)` - which only the VRF program can sign for and which cannot appear as a transaction signer. `draw_round` fixes the callback's account list at request time, so a draw can only reach the round that bought it. The write requires status `Requested`, so it happens once and an expired round cannot be revived, and an all-zero draw is refused because it is indistinguishable from no draw. Reproduced in `localnet/scenarios/c2-unreadable-vrf.ts`. |
| A payout destination that can be locked forever | Payout wallets are `UncheckedAccount` pinned with `address =`, not `SystemAccount`. A system `transfer` only constrains its *source*, but `SystemAccount` insists the destination is System-owned - and a player can irreversibly assign their own wallet away from the System program, which would have made their refund permanently unclaimable. |
| Cancelled push double refunds | Status flip and refund are in one transaction. Failure reverts both. |
| Treasury touching player funds | Two independent guards (accrued bucket + vault balance minus reserved) on `withdraw_protocol_fees`. |
| Star creation called twice | `init` on a deterministic PDA, plus a `next_star_created` latch on the predecessor. |
| Pending pushes rolling forward | `PendingPush.star_id` is written at request time and the star account is seed-bound to it at resolve time. A push can only ever affect the star it was aimed at. |
| Pause / config authority | Neither exists. `Config` has no `authority` and no `paused` field, and no instruction takes an admin signer. |
| Draining `protocol_accrued` through the draw reimbursement | `draw_round` only runs on a round at status `Open` and flips it to `Requested` in the same transaction, and a round never moves backwards. One round, one draw, one reimbursement, ever. |
| Buying a draw nobody will read | `draw_round` requires `star.is_alive()`. A star that died with a funded round behind it used to seal and draw that round out of the float, even though `resolve_push` refunds on `star.is_finished()` before it ever looks at the round. Now those members refund immediately instead of waiting. Reproduced in `localnet/scenarios/m3-dead-star-draw.ts`. |
| Claiming the treasury by front-running the deploy | `initialize` requires the signer to be the address the loader records as the program's upgrade authority, read live out of `ProgramData`. It is the only gated instruction in the program and it is spent on first use. Reproduced in `localnet/scenarios/h2-initialize-race.ts`. |
| A round's rent silently charged to one arbitrary member | `Round.opened_by` records whoever created the account, and `close_round_account` returns the rent to that address once `round.is_drained(star.settle_cursor)` - an exact test, since a round's members are contiguous in `push_id` and the cursor advances one at a time in order. Measured in `localnet/scenarios/m1-rent.ts`. |
| A recycled prize opening a star with no nursery | `create_star` floors the endowment to the push step and caps it at `FEED_MASS - PUSH_STEP`, so an endowed star always has nursery room and its pot always has feeders who could claim it. Without the cap a large enough reserve would birth an unfeedable star whose pot recycles again, ratcheting an unwinnable prize forward. |
| Crank overcharging for randomness | It cannot name an amount. The reimbursement is measured as the cranker's own balance delta across the VRF CPI, capped at `protocol_accrued`. |
| A crank paying the request fee into its own pocket | Measuring the balance delta means a fee that went somewhere the cranker controls would be reimbursed all the same. Anybody may stand up a MagicBlock oracle queue with their own signature, and the fee is paid *into* the queue, so `draw_round` pins `vrf_queue` to `vrf::VRF_QUEUE`. Without it a cranker could file against a queue they own, take the reimbursement, and recover the fee by closing it. This is the direct successor of the ORAO adapter's treasury pin and it protects the same thing: the house's float. |
| Ordinary traffic reverting a draw that is already in flight | `round.entropy` is written once, by the push that opened the round, and every other seed input is either fixed then or chosen by the caller. It used to accumulate over every arrival, which made the ORAO randomness address a function of mempool ordering: any push confirming in the gap failed the crank's draw, worst on the busiest stars, with the batch expiring into refunds if nobody won a gap. There is no derived address left to miss, so what the frozen field now protects is the off-chain derivation the crank uses to match a queued request to its round and the clients use to replay a roll. Reproduced in `localnet/scenarios/h3-draw-race.ts`. |
| Sweeping the float the game buys randomness with | `withdraw_protocol_fees` stays permissionless only down to `DRAW_FLOAT_FLOOR`; below it the treasury has to sign. Nothing about a sweep ever misdirected money - the destination is frozen - but at zero float a third-party crank pays for every draw itself, which is the incentive the game's liveness rests on. Reproduced in `localnet/scenarios/m4-float-floor.ts`. |
| Resolver funding accounts it never gets back | It doesn't. `StarFeed` and `FeedShare` are created and paid for by `request_push`, so `resolve_push` initialises nothing. |
| Vault reaped for rent | Funded to the rent-exempt minimum at `initialize`, and withdrawals reserve it. |

---

## Before mainnet

Known gaps and deliberate simplifications, roughly in priority order.

1. **No external audit.** There has been an internal one, and everything it
   found is fixed and pinned by a scenario in [`localnet/`](./localnet/README.md)
   that runs against a real validator with a mock oracle in MagicBlock's place.
   That is coverage of the paths that were known to be wrong, not of the paths
   nobody has thought of yet - which is what an audit is for. Nothing here has
   been reviewed by anyone outside the project, and none of it has held real
   money.
2. **Upgrade authority is the remaining trust surface - close it on mainnet.**
   Game numbers are compiled in (`game_config.rs`); there is no `update_config`.
   Mainnet's authority is currently the Squads multisig
   `9BfEudxsWmyPP6uGHRyyMHptShK6DVufZSkYd8aaHAdx`, so the program is upgradeable
   today and should be read as one. After you have verified the mainnet deploy,
   make it immutable:

   ```bash
   solana program set-upgrade-authority CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S --final
   ```

   The vault holds the authority, so that has to be executed through Squads
   rather than from a workstation - same constraint as any other upgrade, see
   [Upgrading mainnet](#upgrading-mainnet). It is irreversible. Do not run it on
   the current devnet deployment if you still want to iterate. Treasury is frozen
   at initialize; anyone can crank `withdraw_protocol_fees` to that address, down
   to the draw float floor.
3. **A resolve can be delayed, not corrupted, and no longer indefinitely.**
   Nobody can change an outcome, and a stalled round is now voidable by anyone
   after `ROUND_EXPIRY_SLOTS` so escrow always has an exit. What is still worth
   running is at least one crank, because until someone seals and draws a round
   its members are waiting rather than losing.
4. **The draw float has to be kept topped up.** `draw_round` reimburses
   `cost.min(protocol_accrued)`, so it can never fail - but a house crank running
   against a starved `protocol_accrued` is paying for draws out of its own
   pocket. `fund_protocol` seeds it at genesis; after that the rake refills it as
   long as volume clears the break-even (~0.016 SOL of stake per round at 314
   bps, two minimum pushes). `DRAW_FLOAT_FLOOR` stops a stranger sweeping it to
   zero, but it is a floor, not a budget: the house can still sign that last
   0.05 SOL away, and a star quiet enough to earn nothing will drain it. Watch
   `yarn show-config`.
5. **MagicBlock's oracles are a trusted set**, not a trustless beacon. They
   produce RFC 9381 proofs the VRF program verifies, so a draw cannot be forged,
   but the oracles could in principle collude on what to answer - or simply
   withhold. Withholding is covered: `ROUND_EXPIRY_SLOTS` plus `expire_round` and
   `collapse_stalled_star` refund on slots alone, with no oracle involvement.
   Collusion is not, so evaluate it against the jackpot size and consider a
   second source.
6. **Unclaimed prizes are owed forever.** `prize_liability` never decays, so a
   winner who loses their key permanently locks that SOL in the vault. It is
   correct, but mainnet probably wants an expiry after which an unclaimed prize
   rolls into the next star or the treasury. The same bucket also collects the
   rounding dust from hole shares - `claim_hole_share` floors each share so the
   pot can never come up short for the last claimant, which leaves at most one
   lamport per feeder behind with no path out. Whatever cleans up the first
   should clean up the second.
7. **Batching trades latency for cost.** A push now waits for its round's window
   to close (up to `ROUND_WINDOW_SLOTS`, ~30s) before the draw is even bought. That
   is the price of not charging players for randomness, and it is why the round
   also closes early at `ROUND_TARGET_MEMBERS`. If the game gets busy enough that
   rounds fill instantly this cost disappears; if it is quiet, a lone pusher waits
   the full window.
8. **Round membership is visible in the mempool.** Nothing can be stolen - the
   seed does not exist until sealing and a member's roll depends on their own
   `push_id` - but a determined attacker can still spam a star. Rate limiting is a
   frontend/RPC concern for now.
9. **No `Star` account close path.** Stars accumulate rent forever (~0.0036 SOL
   each). That is intentional - they are the permanent record the lifecycle
   collectible will be minted from - but it is a real cost to budget for.
10. **`close_push` is permissionless.** Rent always returns to the original
    pusher, so this is safe, but it means push history can be cleared by anyone.
    The events are the durable record.
11. **No NFT minting.** Everything needed to mint a lifecycle collectible later
    is preserved on `Star`: seed, final mass, killer, prize, death randomness,
    roll, threshold, timestamps and slots.
