//! Events. The frontend and any indexer should reconstruct the lifecycle from
//! these rather than from account history, so `Star` never has to grow a log.
//!
//! Mapping to the names the Three.js app already thinks in:
//! | frontend        | event                          |
//! |-----------------|--------------------------------|
//! | StarCreated     | [`StarCreated`]                |
//! | PushPending     | [`PushRequested`]              |
//! | PushSurvived    | [`PushResolved`] survived=true |
//! | StageChanged    | [`StageChanged`]               |
//! | StarDestroyed   | [`StarDestroyed`]              |
//! | StarCollapsed   | [`StarCollapsed`]              |
//! | StarStalled     | [`StarStalled`]                |
//! | PushRefunded    | [`PushCancelled`]              |
//! | (round sealed)  | [`RoundClosed`]                |
//! | (oracle bought) | [`RoundRequested`]             |
//! | (round voided)  | [`RoundExpired`]               |

use anchor_lang::prelude::*;

#[event]
pub struct StarCreated {
    pub star_id: u64,
    pub star: Pubkey,
    /// Procedural visual seed.
    pub seed: [u8; 32],
    /// Star id this seed was derived from; 0 for the genesis star.
    pub derived_from_star_id: u64,
    pub birth_ts: i64,
    pub birth_slot: u64,
    /// Inherited jackpot/mass from the previous star's next-star reserve.
    pub endowment: u64,
}

#[event]
pub struct PushRequested {
    pub star_id: u64,
    pub push_id: u64,
    /// The draw this push will share. Its seed does not exist yet.
    pub round_id: u64,
    pub push: Pubkey,
    pub round: Pubkey,
    pub player: Pubkey,
    pub amount: u64,
    /// Published so the round's seed can be replayed from events alone: the one
    /// belonging to the round's first member *is* its entropy.
    pub client_seed: [u8; 32],
    /// How many pushes were sharing the round at this point, including this
    /// one. The oracle cost is divided by its final value.
    pub round_members: u64,
    pub requested_ts: i64,
    pub requested_slot: u64,
    /// Nova chance quoted against committed mass at request time, for display
    /// while the push is in flight. `PushResolved.threshold_ppb` is the one
    /// that was actually rolled against.
    pub chance_ppb: u32,
}

/// A round was sealed and its VRF seed fixed. Emitted by `draw_round`.
///
/// Everything needed to verify the seed independently is here: the sampled slot
/// is named, and the members' contributions are in their `PushRequested`
/// events. No draw exists at this point.
#[event]
pub struct RoundClosed {
    pub star_id: u64,
    pub round_id: u64,
    pub round: Pubkey,
    pub seed: [u8; 32],
    /// The slot whose hash went into `seed`.
    pub seed_slot: u64,
    pub member_count: u64,
    pub first_push_id: u64,
    /// Total stake behind the draw, and the rake it will yield. Published so the
    /// economic gate is auditable after the fact: `rake >= draw_cost` held here,
    /// or the star was full and took its last draw on the house.
    pub stake: u64,
    pub rake: u64,
    pub draw_cost: u64,
    /// True when the round was sealed under the full-star exemption, i.e. it
    /// could not grow any further so waiting for a bigger batch was impossible.
    pub forced: bool,
    pub closed_slot: u64,
    pub closed_ts: i64,
}

/// The one draw for a whole round was bought. Emitted by `draw_round`, the only
/// place the protocol pays the oracle.
#[event]
pub struct RoundRequested {
    pub star_id: u64,
    pub round_id: u64,
    pub round: Pubkey,
    /// Whoever fronted the request fee and was reimbursed.
    pub payer: Pubkey,
    /// What the request actually cost, measured as the payer's balance delta
    /// across the CPI rather than assumed. There is no per-request account to
    /// get rent back from, so this is the whole of it.
    pub cost: u64,
    /// What the rake could actually cover. Below `cost` only when the protocol
    /// balance is short, in which case `payer` absorbed the difference.
    pub reimbursed: u64,
    pub member_count: u64,
    /// `cost / member_count`: what randomness worked out at per push.
    pub cost_per_member: u64,
    pub requested_slot: u64,
}

/// The oracle answered and its draw is now on the round. Emitted by
/// `consume_randomness`, which the VRF program invokes.
///
/// This is the signal a crank waits on before resolving a round's members, and
/// the public record of the draw itself: `randomness` here, widened to 64 bytes,
/// is what every member's roll is derived from, so anyone can replay
/// `sha256("soldust:roll" || randomness || push_id)` and check their own
/// outcome.
#[event]
pub struct RoundDrawn {
    pub star_id: u64,
    pub round_id: u64,
    pub round: Pubkey,
    /// The seed this draw answers, as published in `RoundClosed`.
    pub seed: [u8; 32],
    /// The oracle's 32-byte output. Right-padded to 64 before it reaches the
    /// roll; see `vrf::widen`.
    pub randomness: [u8; 32],
    pub member_count: u64,
    pub first_push_id: u64,
    pub requested_slot: u64,
    pub drawn_slot: u64,
    pub drawn_ts: i64,
}

/// A round stalled and was voided. Emitted by `expire_round`. Every member is
/// now refundable through `resolve_push`, in `push_id` order like any other
/// resolution.
#[event]
pub struct RoundExpired {
    pub star_id: u64,
    pub round_id: u64,
    pub round: Pubkey,
    pub member_count: u64,
    pub first_push_id: u64,
    /// True when the round never even got sealed, i.e. nobody cranked it.
    pub never_sealed: bool,
    pub expired_slot: u64,
    pub expired_ts: i64,
}

/// A fully resolved round's account was closed and its rent handed back to the
/// member who opened it. Emitted by `close_round_account`.
#[event]
pub struct RoundSwept {
    pub star_id: u64,
    pub round_id: u64,
    pub round: Pubkey,
    /// The round's first member, who paid its rent without choosing to.
    pub rent_recipient: Pubkey,
    pub lamports: u64,
    pub member_count: u64,
}

/// A push settled against a live star.
///
/// Field order up to `threshold_ppb` is pinned by the Android log parser; see
/// the layout test in this module.
#[event]
pub struct PushResolved {
    pub star_id: u64,
    pub push_id: u64,
    pub round_id: u64,
    pub push: Pubkey,
    pub player: Pubkey,
    pub amount: u64,
    /// Stake handed back in this same transaction because the room left at
    /// settle was below what the queue quoted at request time. `amount` is
    /// already net of it.
    pub stake_refund: u64,
    /// false means this push destroyed the star.
    pub survived: bool,
    pub roll_ppb: u32,
    /// `amount / star_mass` in ppb: the push's share of the mass it created.
    pub threshold_ppb: u32,
    /// Star mass after this push landed.
    pub star_mass: u64,
    pub star_prize_pool: u64,
    pub stardust_awarded: u64,
    pub resolved_ts: i64,
    pub resolved_slot: u64,
}

#[event]
pub struct PushCancelled {
    pub star_id: u64,
    pub push_id: u64,
    pub round_id: u64,
    pub push: Pubkey,
    pub player: Pubkey,
    /// Refunded in full, in the same transaction.
    pub amount: u64,
    /// True when the refund is because this push's round was voided rather than
    /// because the star had already finished. The star is still alive in that
    /// case and the player can push again immediately.
    pub round_voided: bool,
    pub resolved_ts: i64,
    pub resolved_slot: u64,
}

#[event]
pub struct StageChanged {
    pub star_id: u64,
    pub from_stage: u8,
    pub to_stage: u8,
    pub star_mass: u64,
}

#[event]
pub struct StarDestroyed {
    pub star_id: u64,
    pub star: Pubkey,
    /// The Star Killer.
    pub killer: Pubkey,
    pub killer_push: Pubkey,
    pub killer_push_id: u64,
    /// The round whose draw killed the star. `randomness` below is that draw,
    /// and the roll is `sha256("soldust:roll" || randomness || killer_push_id)`.
    pub killer_round_id: u64,
    pub final_mass: u64,
    pub final_prize: u64,
    pub successful_pushes: u64,
    pub roll_ppb: u32,
    pub threshold_ppb: u32,
    /// Full VRF output, for independent verification.
    pub randomness: [u8; 64],
    pub death_ts: i64,
    pub death_slot: u64,
}

#[event]
pub struct StarCollapsed {
    pub star_id: u64,
    pub star: Pubkey,
    pub final_mass: u64,
    pub final_prize: u64,
    /// SOL recycled into `next_star_reserve` when `early_volume == 0`. Zero if feeders exist.
    pub echo: u64,
    pub early_volume: u64,
    pub successful_pushes: u64,
    /// True when the star collapsed short of the hole cap because it had run out
    /// of ways to ever be pushed again, rather than by surviving to 21 SOL. Only
    /// reachable after a voided round; see `resolve_push::stranded`.
    pub stranded: bool,
    pub death_ts: i64,
    pub death_slot: u64,
}

#[event]
pub struct HoleShareClaimed {
    pub star_id: u64,
    pub player: Pubkey,
    pub amount: u64,
    pub claimed_ts: i64,
}

#[event]
pub struct PrizeClaimed {
    pub star_id: u64,
    pub winner: Pubkey,
    pub amount: u64,
    pub claimed_ts: i64,
}

#[event]
pub struct ProtocolFeesWithdrawn {
    pub treasury: Pubkey,
    pub amount: u64,
    pub remaining_accrued: u64,
}

/// A star was collapsed for going quiet. The liveness floor: it means the
/// oracle or the game stopped, and nobody's SOL is stuck behind it.
///
/// Distinct from [`StarCollapsed`] because it is not an Event Horizon - the pot
/// is not a jackpot here. Feeders take back the pot's share of their own feeds
/// and the rest recycles, so this event pays nobody a profit.
#[event]
pub struct StarStalled {
    pub star_id: u64,
    pub star: Pubkey,
    pub final_mass: u64,
    /// What feeders may claim in total: `prize_bps` of `early_volume`, i.e. the
    /// pot's share of exactly what they put in.
    pub final_prize: u64,
    /// The remainder of the pot, recycled into `next_star_reserve`. Every
    /// lamport of it was rake-free prize money from settled pushes, and it seeds
    /// the next star rather than the treasury.
    pub recycled: u64,
    pub early_volume: u64,
    pub successful_pushes: u64,
    /// Cluster time of the last mass this star ever gained. `death_ts - this` is
    /// how long it had been quiet, and it is always at least the applicable
    /// stall window.
    pub last_mass_ts: i64,
    pub death_ts: i64,
    pub death_slot: u64,
}

/// Lamports were donated into `protocol_accrued`, which is the float
/// `request_round_vrf` reimburses cranks from.
#[event]
pub struct ProtocolFunded {
    pub payer: Pubkey,
    pub amount: u64,
    pub protocol_accrued: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Android client picks nursery feeds out of raw program logs by
    /// indexing into a serialized `PushResolved` (`chain/Accounts.kt`), so these
    /// offsets are an interface. They are counted from the front of the emitted
    /// blob, which starts with an 8-byte event discriminator.
    ///
    /// Swapping the old `oracle_refund` for `round_id` was deliberately size-
    /// neutral, so `survived` and `threshold_ppb` did not move at all; only the
    /// three fields between `push_id` and `stake_refund` shifted by 8.
    #[test]
    fn push_resolved_layout_is_what_the_log_parser_assumes() {
        const DISC: usize = 8;
        let ev = PushResolved {
            star_id: 0x0101_0101_0101_0101,
            push_id: 0x0202_0202_0202_0202,
            round_id: 0x0303_0303_0303_0303,
            push: Pubkey::new_from_array([0xAA; 32]),
            player: Pubkey::new_from_array([0xBB; 32]),
            amount: 0x0404_0404_0404_0404,
            stake_refund: 0,
            survived: true,
            roll_ppb: 0,
            threshold_ppb: 0x0505_0505,
            star_mass: 0,
            star_prize_pool: 0,
            stardust_awarded: 0,
            resolved_ts: 0,
            resolved_slot: 0,
        };
        let mut bytes = Vec::new();
        ev.serialize(&mut bytes).unwrap();
        let at = |off: usize, len: usize| &bytes[off - DISC..off - DISC + len];

        assert_eq!(at(8, 8), &0x0101_0101_0101_0101u64.to_le_bytes());
        assert_eq!(at(16, 8), &0x0202_0202_0202_0202u64.to_le_bytes());
        assert_eq!(at(32, 32), &[0xAAu8; 32]);
        assert_eq!(at(64, 32), &[0xBBu8; 32]);
        assert_eq!(at(96, 8), &0x0404_0404_0404_0404u64.to_le_bytes());
        assert_eq!(at(112, 1), &[1u8]);
        assert_eq!(at(117, 4), &0x0505_0505u32.to_le_bytes());
        // The parser rejects anything shorter than this before indexing.
        assert!(bytes.len() + DISC >= 121);
    }
}
