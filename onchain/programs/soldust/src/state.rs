//! On-chain accounts.
//!
//! PDA map:
//! * `Config`      `["config"]`                     - singleton, vault accounting and the treasury
//! * vault         `["vault"]`                      - system-owned SOL account, holds every lamport
//! * `Star`        `["star", star_id_le]`           - permanent, never overwritten
//! * `Round`       `["round", star_id_le, round_id_le]` - one VRF draw, shared by a batch of pushes
//! * `PendingPush` `["push", player, client_seed]`  - one per push
//! * `Player`      `["player", wallet]`             - lifetime player stats
//! * `StarFeed`    `["feed", star_id_le]`           - early-feed volume for the hole
//! * `FeedShare`   `["feed-share", star_id_le, wallet]` - one hole ticket

use anchor_lang::prelude::*;

use crate::constants::{
    BPS, FEED_MASS, HOLE_MASS, NURSERY_STALL_SECS, ROUND_EXPIRY_SLOTS, ROUND_TARGET_MEMBERS,
    ROUND_WINDOW_SLOTS, STALL_SECS,
};
use crate::game_config::LifecycleConfig;

#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, PartialEq, Eq, Debug)]
pub enum StarStatus {
    Alive,
    Dead,
    /// Reached Event Horizon without a supernova. Prize is for early feeders.
    BlackHole,
    /// Collapsed after going quiet for [`STALL_SECS`](crate::constants::STALL_SECS).
    /// Pays feeders their own feeds back at cost and recycles the rest, so
    /// nothing is stuck if the oracle dies or the game does. Appended last so
    /// the three discriminants above kept their values for the hand-decoders.
    Stalled,
}

#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushStatus {
    /// Escrowed, not yet settled.
    Pending,
    /// Settled against a live star, star survived.
    Survived,
    /// Settled against a live star, star died. This is the winning push.
    Killed,
    /// Never rolled: either the star was already finished at resolve time, or
    /// this push's round expired without a draw. Stake was returned in full.
    Cancelled,
}

/// Where a [`Round`] is in its life. The order matters: a round only ever
/// moves forwards, and only `Requested` permits a push to settle.
#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoundStatus {
    /// Accepting pushes. Its draw does not exist and its seed is not yet
    /// decidable by anyone.
    Open,
    /// Sealed, and its VRF request filed in the same transaction. The draw has
    /// not arrived yet, so no push may settle here.
    ///
    /// There is deliberately no state between `Open` and this one: the seed is
    /// decided and spent in a single instruction, so it is never public while
    /// still unspent.
    Requested,
    /// Voided after [`ROUND_EXPIRY_SLOTS`] without a usable draw. Every member
    /// refunds in full, in `push_id` order like any other resolution, and the
    /// star lives on with a new round.
    Expired,
    /// The oracle called back and its randomness is on the round. This is the
    /// only status a push may settle under.
    ///
    /// Appended last rather than filed after `Requested` so the existing Borsh
    /// indices did not move - `Open`, `Requested` and `Expired` are still 0, 1
    /// and 2 on the wire. A unit-only enum is one tag byte whatever its variant
    /// count, so this costs no space either. Nothing compares these ordinally;
    /// every reader pattern-matches, so the out-of-order index is invisible.
    ///
    /// The only writer is [`crate::consume_randomness`], which the VRF program
    /// must sign for with [`crate::vrf::callback_identity`]. That is what stops
    /// randomness this program did not ask for from ever being read as a
    /// round's draw, and it replaces the address-derivation check the pull-based
    /// ORAO integration relied on.
    Drawn,
}

/// Singleton game state: where the money is owed, and which star is live.
///
/// It holds no tunable parameters. Odds, bounds and splits are compiled into
/// [`game_config`](crate::game_config) and cannot be changed by any
/// instruction.
#[account]
#[derive(InitSpace, Debug)]
pub struct Config {
    /// Destination for `withdraw_protocol_fees`. Frozen at initialize.
    pub treasury: Pubkey,

    /// 0 until the first star is created.
    pub current_star_id: u64,
    pub stars_created: u64,
    /// Visual seed for star #1. Later stars hash the previous star's seed.
    pub genesis_seed: [u8; 32],

    // ---- vault accounting. These four are the only claims on the vault. ----
    /// Stake escrowed by pushes that have not been settled or refunded yet.
    /// Not part of any jackpot until the push resolves against a live star.
    pub pending_liability: u64,
    /// Prize money owed to (current + past) star killers, claimed or not.
    pub prize_liability: u64,
    /// Protocol revenue, and the float randomness is bought out of. The only
    /// balance `withdraw_protocol_fees` can touch, and the only balance
    /// `request_round_vrf` can reimburse a crank from.
    pub protocol_accrued: u64,
    /// Recycled prize waiting to seed the next star. Not withdrawable.
    pub next_star_reserve: u64,

    // ---- lifetime counters, for convenience ----
    pub total_pushes_settled: u64,
    pub total_volume: u64,

    pub bump: u8,
    pub vault_bump: u8,
}

impl Config {
    /// Everything the vault owes to players. Protocol withdrawals must never
    /// reach into this.
    pub fn reserved(&self) -> Result<u64> {
        let a = crate::math::add(self.pending_liability, self.prize_liability)?;
        crate::math::add(a, self.next_star_reserve)
    }
}

/// Permanent record for one star. Never mutated once dead, apart from the
/// one-shot `prize_claimed` and `next_star_created` latches.
#[account]
#[derive(InitSpace, Debug)]
pub struct Star {
    pub star_id: u64,
    /// Deterministic procedural visual seed. Drives the whole Three.js look.
    pub seed: [u8; 32],
    pub status: StarStatus,
    /// Index into `self.lifecycle.stages`.
    pub stage: u8,

    pub birth_ts: i64,
    pub birth_slot: u64,
    pub death_ts: i64,
    pub death_slot: u64,

    /// Pushes that settled against this star while it was alive and landed
    /// mass. Not the settle cursor - an expired round advances the cursor
    /// without landing anything. See [`Star::settle_cursor`].
    pub successful_pushes: u64,
    /// Requested but not yet resolved.
    pub pending_pushes: u64,
    /// Escrowed lamports of those pending pushes. Reserves room against the
    /// hole cap so the queue cannot commit past it.
    pub pending_lamports: u64,
    /// Arrived after death and were refunded.
    pub cancelled_pushes: u64,
    /// Monotonic per-star push numbering. Assigned at request; settle order
    /// follows it while the star is alive.
    pub push_counter: u64,

    /// Total lamports that successfully landed. Drives the lifecycle stage.
    pub total_mass: u64,
    /// Prize share accumulated so far (`prize_bps` of settled mass), plus any
    /// endowment in full. This is what a kill pays out, and the odds are the
    /// push's share of `total_mass`, so a star with no endowment returns
    /// exactly `prize_bps` and an endowed one returns better.
    pub prize_pool: u64,
    /// Stage table, copied from the compiled lifecycle at birth, so an upgrade
    /// cannot move a running star's STARDUST taper. The odds are not in here:
    /// they are `amount / mass_after`, which no table can change.
    pub lifecycle: LifecycleConfig,

    // ---- set atomically on death ----
    pub killer: Pubkey,
    pub killer_push: Pubkey,
    pub killer_push_id: u64,
    pub final_prize: u64,
    pub prize_claimed: bool,
    pub next_star_created: bool,

    /// Full 64-byte VRF output of the lethal push, kept so anyone can verify
    /// the death independently.
    pub death_randomness: [u8; 64],
    /// The roll that was taken, and what it had to beat.
    pub death_roll_ppb: u32,
    pub death_threshold_ppb: u32,

    // These two are appended rather than filed next to the counters they
    // belong with, because the Android client hand-decodes this account from
    // literal offsets and everything above is load-bearing there. See the
    // layout test at the bottom of this file.
    /// The round `request_push` currently stamps onto new pushes. Bumped by
    /// `close_round`, and by `expire_round` when it voids a round that never
    /// got that far, so a fresh push always lands somewhere still open.
    pub current_round: u64,
    /// The next `push_id` allowed to resolve while the star is alive.
    ///
    /// Every push is processed exactly once and ids are dense from zero, so
    /// this is just "how many have been dealt with" - which makes it an exact
    /// ordering guard. It counts refunds from an expired round as well as
    /// settles, because otherwise a voided batch would wedge every push behind
    /// it forever.
    pub settle_cursor: u64,

    pub bump: u8,

    /// Cluster time when this star last *gained mass* - birth, a feed, or a
    /// settled push. The stall clock, and the only field written purely to prove
    /// liveness.
    ///
    /// Filed after `bump` rather than beside `settle_cursor` because it was
    /// added last and everything above it, `bump` included, sits at an offset the
    /// Android decoder has hardcoded. Appending is the only edit to this account
    /// that cannot break it.
    pub last_mass_ts: i64,
}

impl Star {
    pub fn is_alive(&self) -> bool {
        self.status == StarStatus::Alive
    }

    /// Done, by any of the three routes out. Every caller of this is asking the
    /// same question - "can this star still take or roll a push?" - so a stalled
    /// star has to answer no here exactly like a dead one: it stops drawing,
    /// refunds anything still queued, and lets a successor be born.
    pub fn is_finished(&self) -> bool {
        matches!(self.status, StarStatus::Dead | StarStatus::BlackHole | StarStatus::Stalled)
    }

    /// Settled mass plus stake still in line. Once this reaches the hole
    /// cap the star cannot take another push, even if it has not died yet.
    pub fn committed_mass(&self) -> u64 {
        self.total_mass.saturating_add(self.pending_lamports)
    }

    /// Finished, or already full: play can move on while the queue settles.
    pub fn is_closed(&self) -> bool {
        self.is_finished() || self.committed_mass() >= HOLE_MASS
    }

    /// Full to the hole cap, but still alive.
    ///
    /// The one state in which a round is drawn no matter what it raked: nothing
    /// more can ever join it, so it will never clear the economic bar on its own,
    /// yet its members still need a roll - on a *finished* star they refund
    /// without randomness, so a draw there would buy nothing. Without this the
    /// last fill into a star could never settle and the star could never nova.
    pub fn is_full_and_live(&self) -> bool {
        self.is_alive() && self.committed_mass() >= HOLE_MASS
    }

    /// How long this star may go without gaining mass before anyone may
    /// collapse it.
    ///
    /// The short fuse is for a star still in its nursery, where the pot is just
    /// the feeds themselves and there is no jackpot upside to preserve. Read off
    /// mass rather than a push count: a last feed may overshoot [`FEED_MASS`] by
    /// its own size, so a star that only ever took feeds can land on the long
    /// fuse. That costs its feeders nothing but patience, and it keeps this a
    /// one-line predicate over state anyone can read.
    pub fn stall_secs(&self) -> i64 {
        if self.total_mass > FEED_MASS {
            STALL_SECS
        } else {
            NURSERY_STALL_SECS
        }
    }

    /// Has this star gone quiet long enough to be collapsed?
    ///
    /// Three conditions, and each is load-bearing:
    ///
    /// * **Alive.** A finished star has already paid out.
    /// * **Nothing queued.** `pending_pushes == 0` means no stake is escrowed
    ///   here, so a collapse cannot cut in front of a push that still has a roll
    ///   coming. Anyone can clear the queue first - settles need a draw, but
    ///   [`ROUND_EXPIRY_SLOTS`] refunds do not, which is what makes this
    ///   reachable with the oracle dark.
    /// * **No mass for `stall_secs`.** Mass, not activity: refunds, closed
    ///   rounds and expired rounds all move the star without anyone winning
    ///   anything, and a star that can only do those is exactly the stalled
    ///   case. The stamp advances on settles and feeds only, so any real play at
    ///   all resets the clock.
    ///
    /// Never reads the round. A stall is about the star, and the round it is
    /// pointing at may not even exist yet.
    pub fn is_stalled(&self, now: i64) -> bool {
        self.is_alive()
            && self.pending_pushes == 0
            && now.saturating_sub(self.last_mass_ts) >= self.stall_secs()
    }
}

/// One VRF draw, shared by every push queued while it was open.
///
/// This is what makes the oracle affordable: the oracle charges per request,
/// not per player, so a round of `n` pushes costs `1/n` of a request each. It is also
/// what makes the randomness cheap to *verify* - there is one draw and one
/// published seed per batch, and each member's roll is a labelled hash of it.
///
/// ## Why the seed is safe
///
/// The seed must not be computable by anyone until the round is sealed,
/// otherwise somebody buys the draw early, reads it, and only pushes when the
/// roll would be lethal. Two ingredients, each covering the other's gap:
///
/// * **`entropy`**, committed by the round's first member from their own
///   `client_seed`. A player can steer their own contribution, so this alone is
///   not enough.
/// * **a slot hash sampled at the draw**, which nobody can predict - not the
///   members, and not whoever submits the draw, since the draw that seed
///   produces does not exist yet and cannot be shopped for.
///
/// Members are therefore committed before the seed is decidable, and the seed is
/// decided in the very transaction that spends it, so it is never public while
/// still unspent. Nothing to grind at either end, and nothing to squat on in
/// between.
///
/// Every input is also fixed before a draw is attempted, which is a liveness
/// property rather than a security one: see `entropy` below.
#[account]
#[derive(InitSpace, Debug)]
pub struct Round {
    pub star_id: u64,
    pub round_id: u64,

    /// The player-supplied half of the seed input, written once by the member
    /// who opened the round and never touched again.
    ///
    /// Frozen rather than accumulated because the seed must not move once a
    /// draw can be attempted against it. A field that changed with every
    /// arrival meant any push landing between a crank reading this round and
    /// its draw executing changed the seed under it - no attack needed, just
    /// traffic, and the busier the star the less often a draw could land.
    pub entropy: [u8; 32],
    /// The VRF `caller_seed`, decided by `draw_round` in the same transaction
    /// that buys the randomness. Zero while the round is open, which is the
    /// honest representation: it is genuinely not decided yet.
    pub seed: [u8; 32],
    /// The draw itself, written by [`crate::consume_randomness`] when the
    /// oracle calls back. Zero until then.
    ///
    /// This field held a `Pubkey` under the pull-based ORAO integration - the
    /// address of the randomness account to read - and a `Pubkey` is 32 raw
    /// Borsh bytes, exactly what MagicBlock delivers. So the push model reuses
    /// the slot in place: same offset, same `INIT_SPACE`, same account size, no
    /// migration. `status` is what says whether these bytes mean anything, not
    /// their value.
    ///
    /// Widened to 64 bytes by [`crate::vrf::widen`] before it reaches the roll,
    /// which is defined over the shape ORAO used to deliver.
    pub randomness: [u8; 32],
    /// Slot whose hash went into `seed`, so the derivation can be replayed
    /// off-chain from public data alone.
    pub seed_slot: u64,

    /// How many pushes share this draw. The divisor on the oracle cost.
    pub member_count: u64,
    /// Total escrowed stake of those members. Not accounting - this is what
    /// decides whether the round has earned its draw yet. See
    /// [`Round::covers_draw`].
    pub stake: u64,
    /// `push_id` of the first member, so a client can enumerate the batch.
    pub first_push_id: u64,

    pub status: RoundStatus,
    /// Slot the first member arrived, i.e. when the window started.
    pub opened_slot: u64,
    /// Slot the round stopped taking members. Same slot as `requested_slot`,
    /// since sealing and drawing are one instruction.
    pub closed_slot: u64,
    pub requested_slot: u64,
    pub opened_ts: i64,

    pub bump: u8,

    /// Whoever paid this account's rent by being its first member, so
    /// [`close_round_account`](crate::close_round_account) can hand it back once
    /// the round has drained. Appended last on purpose: no existing field moved.
    pub opened_by: Pubkey,
}

impl Round {
    pub fn is_open(&self) -> bool {
        self.status == RoundStatus::Open
    }

    /// A round may be sealed once its window has run, or as soon as it is full
    /// enough that waiting only adds latency without saving anything.
    ///
    /// Empty rounds are never closeable: closing one would buy a draw nobody
    /// is waiting on, and the account is created by its first member anyway.
    /// Neither is anything already sealed or voided - a seed is fixed once.
    pub fn closeable_at(&self, slot: u64) -> bool {
        if !self.is_open() || self.member_count == 0 {
            return false;
        }
        self.member_count >= ROUND_TARGET_MEMBERS
            || slot >= self.opened_slot.saturating_add(ROUND_WINDOW_SLOTS)
    }

    /// The slot this round last made progress from. Expiry is measured from
    /// here, so each stage gets the full grace period rather than sharing one.
    pub fn stalled_since(&self) -> u64 {
        match self.status {
            RoundStatus::Open => self.opened_slot,
            RoundStatus::Requested => self.requested_slot,
            // Terminal: neither has a stall to measure. The sentinel is only a
            // belt to `expirable_at`'s braces, which refuses both statuses
            // outright - `slot >= u64::MAX` is *true* at `slot == u64::MAX`, so
            // a sentinel on its own would not actually hold at the boundary.
            RoundStatus::Drawn | RoundStatus::Expired => u64::MAX,
        }
    }

    /// Both terminal statuses are refused here rather than left to the
    /// arithmetic. `Expired` because voiding twice would refund twice, and
    /// `Drawn` because a landed draw is a usable draw however late it was -
    /// letting one be voided is precisely how a member would duck an
    /// unfavourable roll, so it must not depend on a slot counter never
    /// reaching its maximum.
    pub fn expirable_at(&self, slot: u64) -> bool {
        if self.member_count == 0
            || self.status == RoundStatus::Expired
            || self.status == RoundStatus::Drawn
        {
            return false;
        }
        slot >= self.stalled_since().saturating_add(ROUND_EXPIRY_SLOTS)
    }

    /// The protocol cut this round's members will hand over when they settle.
    ///
    /// Computed with the same `protocol_bps` the settle itself uses, so this is
    /// the actual figure rather than a proxy for it.
    pub fn rake(&self, protocol_bps: u16) -> u64 {
        ((self.stake as u128).saturating_mul(protocol_bps as u128) / BPS as u128) as u64
    }

    /// Has this round earned the draw it is about to buy?
    ///
    /// The entire batching bet is that `n` members split one request, so the
    /// question is never "can one push afford randomness" - it is "is this batch
    /// worth a request yet". A round that has not cleared the bar is simply left
    /// open to keep collecting members; see `close_round`.
    ///
    /// This is what makes the house's edge structural rather than hopeful. Every
    /// draw it ever buys is already paid for by the stake behind that draw, so
    /// the float cannot be bled by volume - however small the pushes are, and
    /// whatever the oracle charges. The cost is a parameter here rather than a
    /// constant precisely so that swapping oracles reprices the bar instead of
    /// invalidating the argument.
    pub fn covers_draw(&self, draw_cost: u64, protocol_bps: u16) -> bool {
        self.rake(protocol_bps) >= draw_cost
    }

    /// Has every member of this round resolved?
    ///
    /// Exact without a per-round counter, because a round's members are
    /// *contiguous* in `push_id`: `request_push` assigns ids from one sequence and
    /// puts each new push in whatever round is open at the time, so a round owns
    /// the half-open range `[first_push_id, first_push_id + member_count)` and
    /// nothing else. `settle_cursor` is a watermark over that same sequence -
    /// every resolution, refund included, advances it by exactly one and only in
    /// order - so it having passed the end of the range means every id inside it
    /// is done.
    pub fn is_drained(&self, settle_cursor: u64) -> bool {
        settle_cursor >= self.first_push_id.saturating_add(self.member_count)
    }
}

/// Early-feed tally for one star. Created on the first push.
#[account]
#[derive(InitSpace, Debug)]
pub struct StarFeed {
    pub star_id: u64,
    /// Settled lamports pushed while mass was still below 1 SOL.
    pub early_volume: u64,
    pub bump: u8,
}

/// One wallet's hole ticket on one star.
#[account]
#[derive(InitSpace, Debug)]
pub struct FeedShare {
    pub star_id: u64,
    pub player: Pubkey,
    pub amount: u64,
    pub claimed: bool,
    pub bump: u8,
}

/// One push. Created when the player signs, permanently bound to the star
/// they meant to push and to the round whose draw decides its fate.
///
/// Field order up to `amount` is fixed by the Android hand-decoder, same as
/// [`Star`]; see the layout test at the bottom of this file.
#[account]
#[derive(InitSpace, Debug)]
pub struct PendingPush {
    /// The star this push was aimed at. Never rolls forward to a later star.
    pub star_id: u64,
    pub push_id: u64,
    pub player: Pubkey,
    /// Escrowed stake, in lamports.
    pub amount: u64,
    /// The draw this push shares. Recorded at request time, before that draw's
    /// seed is decidable, which is the entire anti-grinding argument. Filed
    /// after `amount` rather than next to `push_id` so the two offsets above
    /// stayed where the hand-decoder already looks.
    pub round_id: u64,
    pub status: PushStatus,

    pub requested_ts: i64,
    pub requested_slot: u64,
    pub resolved_ts: i64,
    pub resolved_slot: u64,

    /// Populated on a settled (non-cancelled) resolve.
    pub roll_ppb: u32,
    pub threshold_ppb: u32,
    /// Nova chance quoted at request time, against committed mass. For
    /// display only: the authoritative threshold is computed at settle from
    /// settled mass and written to `threshold_ppb`. The two differ when the
    /// queue moved in between, and the difference never changes the push's
    /// expected value.
    pub chance_ppb: u32,
    /// Stage dust multiplier frozen at request (queue-aware stage).
    pub stardust_mult_bps: u16,

    /// The second half of this account's own address, and - for the member who
    /// opened the round - the player-supplied half of its seed. Appended rather
    /// than filed next to `round_id` so the hand-decoded offsets above did not
    /// move.
    ///
    /// Kept on chain rather than only in the request event so that a round's seed
    /// is verifiable from account state alone: fold the `client_seed` of the push
    /// at `round.first_push_id`, mix in the slot hash `round.seed_slot` names and
    /// the payer on `RoundClosed`, and the result has to equal `round.seed`.
    pub client_seed: [u8; 32],

    pub bump: u8,
}

impl PendingPush {
    pub fn is_pending(&self) -> bool {
        self.status == PushStatus::Pending
    }
}

/// Lifetime stats per wallet. STARDUST is internal points only - there is
/// deliberately no SPL token in this MVP.
#[account]
#[derive(InitSpace, Debug)]
pub struct Player {
    pub wallet: Pubkey,
    pub stardust: u64,
    pub requested_pushes: u64,
    pub successful_pushes: u64,
    pub cancelled_pushes: u64,
    pub total_sol_pushed: u64,
    pub stars_killed: u64,
    pub prizes_won: u64,
    pub bump: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{LAMPORTS_PER_SOL, PUSH_STEP};

    fn round(status: RoundStatus, members: u64) -> Round {
        Round {
            star_id: 1,
            round_id: 0,
            entropy: [0; 32],
            seed: [0; 32],
            randomness: [0; 32],
            seed_slot: 0,
            member_count: members,
            stake: 0,
            first_push_id: 0,
            status,
            opened_slot: 1_000,
            closed_slot: 0,
            requested_slot: 0,
            opened_ts: 0,
            bump: 0,
            opened_by: Pubkey::default(),
        }
    }

    /// The batching window: nobody waits longer than it, and a round that is
    /// already big enough to have amortised its draw does not wait at all.
    #[test]
    fn a_round_seals_on_the_window_or_on_being_full() {
        let r = round(RoundStatus::Open, 1);
        assert!(!r.closeable_at(1_000), "sealed in the same slot it opened");
        assert!(!r.closeable_at(1_000 + ROUND_WINDOW_SLOTS - 1));
        assert!(r.closeable_at(1_000 + ROUND_WINDOW_SLOTS));

        // Full enough to skip the wait entirely.
        let full = round(RoundStatus::Open, ROUND_TARGET_MEMBERS);
        assert!(full.closeable_at(1_000));
    }

    /// An empty round must never be sealed: it would buy a draw nobody is
    /// waiting on, straight out of the rake.
    #[test]
    fn an_empty_round_is_never_sealed_or_voided() {
        let r = round(RoundStatus::Open, 0);
        assert!(!r.closeable_at(u64::MAX));
        assert!(!r.expirable_at(u64::MAX));
    }

    /// The freeze escape hatch. Each stage gets its own full grace period,
    /// measured from when that stage began, so a round that stalls late is not
    /// punished for having made progress early.
    #[test]
    fn every_stall_point_expires_from_its_own_clock() {
        for (status, since) in [(RoundStatus::Open, 1_000u64), (RoundStatus::Requested, 3_000)] {
            let mut r = round(status, 3);
            r.closed_slot = 3_000;
            r.requested_slot = 3_000;
            assert_eq!(r.stalled_since(), since);
            assert!(!r.expirable_at(since + ROUND_EXPIRY_SLOTS - 1));
            assert!(r.expirable_at(since + ROUND_EXPIRY_SLOTS));
        }
    }

    /// The counterpart, and the property that replaced parsing a foreign oracle
    /// account: once the callback has landed the round is out of the expiry
    /// path entirely, so nobody can look at an unfavourable draw and void it.
    #[test]
    fn a_drawn_round_can_never_be_voided() {
        let mut r = round(RoundStatus::Drawn, 3);
        r.requested_slot = 3_000;
        assert_eq!(r.stalled_since(), u64::MAX);
        assert!(!r.expirable_at(u64::MAX));
        assert!(!r.closeable_at(u64::MAX));
    }

    /// Voiding is terminal. Re-voiding would refund a member twice if the
    /// handler ever stopped checking, so the predicate refuses on its own.
    #[test]
    fn a_voided_round_cannot_be_voided_again() {
        let r = round(RoundStatus::Expired, 3);
        assert!(!r.expirable_at(u64::MAX));
        assert!(!r.closeable_at(u64::MAX));
    }

    /// Only an open round takes members, so a sealed seed can never acquire one.
    #[test]
    fn only_an_open_round_is_open() {
        assert!(round(RoundStatus::Open, 1).is_open());
        for s in [
            RoundStatus::Requested,
            RoundStatus::Expired,
            RoundStatus::Drawn,
        ] {
            assert!(!round(s, 1).is_open());
        }
    }

    /// The invariant the whole oracle swap rests on: reinterpreting
    /// `randomness` from a `Pubkey` to `[u8; 32]` must not move a byte, because
    /// the deployed program is upgraded in place over live `Round` accounts.
    /// Both are 32 raw Borsh bytes at the same offset, so the total must still
    /// be what it was under ORAO.
    #[test]
    fn the_round_layout_did_not_move() {
        assert_eq!(Round::INIT_SPACE, 210);
        assert_eq!(RoundStatus::INIT_SPACE, 1, "a unit enum is one tag byte");
    }

    /// The rent-reclaim test. A round is done exactly when the settle cursor has
    /// passed the last of its contiguous member ids - one short and its rent must
    /// stay, because a member could still need the account to resolve against.
    #[test]
    fn a_round_is_drained_only_once_its_last_member_resolved() {
        let mut r = round(RoundStatus::Requested, 3);
        r.first_push_id = 7;

        assert!(!r.is_drained(7), "nothing resolved yet");
        assert!(!r.is_drained(9), "the last member is still pending");
        assert!(r.is_drained(10), "cursor past first_push_id + member_count");
        assert!(r.is_drained(11), "and it never un-drains");
    }

    /// MagicBlock's request fee, which is the whole of what a draw costs now
    /// that there is no per-request account entombing rent.
    const DRAW_COST: u64 = crate::vrf::VRF_REQUEST_FEE;

    /// The economic gate. A round is drawable once its own rake pays for the
    /// draw, and one minimum push is still not enough - which is the entire
    /// reason rounds are held open rather than sealed on the window alone.
    #[test]
    fn a_round_must_pay_for_its_own_draw() {
        let bps = crate::game_config::economics().protocol_bps;
        let mut r = round(RoundStatus::Open, 1);

        r.stake = PUSH_STEP;
        assert!(
            !r.covers_draw(DRAW_COST, bps),
            "one minimum push cannot buy a draw; if it could there would be \
             nothing to batch and the gate could be deleted"
        );

        // Two minimum pushes clear it, one does not. Under ORAO's
        // 2_178_245-lamport draw this took seven, which is what made quiet
        // stars slow: the shortfall held the round open long past its window.
        r.stake = 2 * PUSH_STEP;
        assert!(r.covers_draw(DRAW_COST, bps));

        // One large push needs no company at all.
        r.stake = LAMPORTS_PER_SOL;
        assert!(r.covers_draw(DRAW_COST, bps));
    }

    /// The bar is a pure function of the price, so a reprice moves it rather
    /// than breaking anything: rounds just need more members.
    #[test]
    fn the_bar_tracks_whatever_the_oracle_charges() {
        let bps = crate::game_config::economics().protocol_bps;
        let mut r = round(RoundStatus::Open, 1);
        r.stake = LAMPORTS_PER_SOL;

        assert!(r.covers_draw(r.rake(bps), bps), "exactly break-even passes");
        assert!(!r.covers_draw(r.rake(bps) + 1, bps));
        assert!(r.covers_draw(0, bps), "a free draw is always worth taking");
    }

    /// Why `close_round` needs the full-star exemption. The last legal push into
    /// a star is a single step, its round can never grow because committed mass
    /// has hit the cap, and a single step does not clear the bar - so without an
    /// exemption that push could never be drawn and the star could never take
    /// its final roll.
    #[test]
    fn the_final_push_into_a_star_can_never_clear_the_bar_alone() {
        let bps = crate::game_config::economics().protocol_bps;
        let mut r = round(RoundStatus::Open, 1);
        r.stake = PUSH_STEP;
        assert!(!r.covers_draw(DRAW_COST, bps));
    }

    /// The window has to be long enough to batch and short enough not to be
    /// noticed, and expiry has to be far longer than the window or a busy star
    /// would void rounds it was merely slow to seal.
    #[test]
    fn round_timings_are_sane_relative_to_each_other() {
        assert!(ROUND_WINDOW_SLOTS >= 4, "window too short to batch anything");
        assert!(
            ROUND_WINDOW_SLOTS >= 60,
            "window under ~24s still feels like a race"
        );
        assert!(
            ROUND_WINDOW_SLOTS <= 90,
            "window over ~36s is a dead wait on a quiet star"
        );
        assert!(
            ROUND_EXPIRY_SLOTS >= ROUND_WINDOW_SLOTS * 10,
            "expiry must dwarf the window"
        );
        assert!(ROUND_TARGET_MEMBERS >= 2);
    }

    fn star(status: StarStatus, total_mass: u64, last_mass_ts: i64) -> Star {
        Star {
            star_id: 1,
            seed: [0; 32],
            status,
            stage: 0,
            birth_ts: 0,
            birth_slot: 0,
            death_ts: 0,
            death_slot: 0,
            successful_pushes: 0,
            pending_pushes: 0,
            pending_lamports: 0,
            cancelled_pushes: 0,
            push_counter: 0,
            total_mass,
            prize_pool: 0,
            lifecycle: LifecycleConfig::default(),
            killer: Pubkey::default(),
            killer_push: Pubkey::default(),
            killer_push_id: 0,
            final_prize: 0,
            prize_claimed: false,
            next_star_created: false,
            death_randomness: [0; 64],
            death_roll_ppb: 0,
            death_threshold_ppb: 0,
            current_round: 0,
            settle_cursor: 0,
            bump: 0,
            last_mass_ts,
        }
    }

    /// The liveness floor. A star is collapsible only when all three hold, and
    /// each one is somebody's protection: alive (it has not already paid), empty
    /// (no push is waiting on a roll this would cancel), quiet (nobody is
    /// playing).
    #[test]
    fn a_star_collapses_only_when_alive_empty_and_quiet() {
        let played = FEED_MASS + PUSH_STEP;
        let s = star(StarStatus::Alive, played, 1_000);
        assert!(!s.is_stalled(1_000), "stalled the instant mass landed");
        assert!(!s.is_stalled(1_000 + STALL_SECS - 1), "a second early");
        assert!(s.is_stalled(1_000 + STALL_SECS));

        let mut queued = star(StarStatus::Alive, played, 1_000);
        queued.pending_pushes = 1;
        assert!(
            !queued.is_stalled(i64::MAX),
            "collapsed over the top of a push that still had a roll coming"
        );

        for status in [StarStatus::Dead, StarStatus::BlackHole, StarStatus::Stalled] {
            assert!(
                !star(status, played, 1_000).is_stalled(i64::MAX),
                "{status:?} collapsed twice"
            );
        }
    }

    /// A star still in its nursery has no jackpot to protect - the pot is the
    /// feeds - so it gets the day fuse instead of the week.
    #[test]
    fn a_nursery_star_gets_the_short_fuse() {
        assert_eq!(star(StarStatus::Alive, 0, 0).stall_secs(), NURSERY_STALL_SECS);
        assert_eq!(
            star(StarStatus::Alive, FEED_MASS, 0).stall_secs(),
            NURSERY_STALL_SECS
        );
        assert_eq!(
            star(StarStatus::Alive, FEED_MASS + PUSH_STEP, 0).stall_secs(),
            STALL_SECS
        );
    }

    /// Guard on the localnet build. `short-stalls` shrinks these to seconds so a
    /// scenario can reach a collapse; if that build could also pass the suite,
    /// nothing would stop it reaching a cluster. It cannot: these two asserts
    /// only compile when the feature is off.
    #[cfg(not(feature = "short-stalls"))]
    #[test]
    fn shipped_stall_windows_are_days() {
        assert_eq!(NURSERY_STALL_SECS, 24 * 60 * 60);
        assert_eq!(STALL_SECS, 7 * 24 * 60 * 60);
    }

    /// The Android client hand-decodes `Star` from raw account bytes with
    /// literal offsets (`chain/Accounts.kt`), because it has no Anchor
    /// codegen. Anything that moves a field silently breaks it at runtime,
    /// so the sizes it derives its offsets from are pinned here.
    #[test]
    fn star_layout_is_what_the_hand_decoder_assumes() {
        // stage_count + 7 × (min_mass u64 + stardust_mult_bps u16).
        assert_eq!(LifecycleConfig::INIT_SPACE, 1 + 7 * 10);
        // Offsets in the kotlin decoder, counted from the front of the account
        // data so they include the 8-byte discriminator: pending_lamports 98,
        // push_counter 114, total_mass 122, prize_pool 130, lifecycle 138,
        // killer 209, final_prize 281, prize_claimed 289.
        //
        // `current_round` and `settle_cursor` were appended after
        // `death_threshold_ppb` precisely so none of the above moved: 356 + 16.
        // `last_mass_ts` went in after `bump` for the same reason: 372 + 8.
        assert_eq!(Star::INIT_SPACE, 380);
        // A fourth `StarStatus` is still one byte, so nothing after `status`
        // shifted when `Stalled` was added.
        assert_eq!(StarStatus::INIT_SPACE, 1);
    }

    /// Same deal for `PendingPush`, but checked against real serialized bytes
    /// rather than a size, because these are the four offsets the decoder
    /// actually indexes.
    ///
    /// Dropping the per-push seed, randomness and oracle budget shortened this
    /// a lot. `round_id` went in after `amount` specifically so `player` and
    /// `amount` stayed where the decoder already looks; only `status` moved.
    #[test]
    fn push_layout_is_what_the_hand_decoder_assumes() {
        const DISC: usize = 8;
        let push = PendingPush {
            star_id: 0x0101_0101_0101_0101,
            push_id: 0x0202_0202_0202_0202,
            round_id: 0x0303_0303_0303_0303,
            player: Pubkey::new_from_array([0xAA; 32]),
            amount: 0x0404_0404_0404_0404,
            status: PushStatus::Cancelled,
            requested_ts: 0,
            requested_slot: 0,
            resolved_ts: 0,
            resolved_slot: 0,
            roll_ppb: 0,
            threshold_ppb: 0,
            chance_ppb: 0,
            stardust_mult_bps: 0,
            client_seed: [0; 32],
            bump: 0,
        };
        let mut bytes = Vec::new();
        push.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), PendingPush::INIT_SPACE);

        let at = |off: usize, len: usize| &bytes[off - DISC..off - DISC + len];
        assert_eq!(at(8, 8), &0x0101_0101_0101_0101u64.to_le_bytes());
        assert_eq!(at(24, 32), &[0xAAu8; 32]);
        assert_eq!(at(56, 8), &0x0404_0404_0404_0404u64.to_le_bytes());
        assert_eq!(at(64, 8), &0x0303_0303_0303_0303u64.to_le_bytes());
        assert_eq!(at(72, 1), &[PushStatus::Cancelled as u8]);
    }
}
