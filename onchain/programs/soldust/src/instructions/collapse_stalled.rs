//! The liveness floor: a star that stops moving can be finished by anyone.
//!
//! Everything else in this program recovers from a *transient* failure.
//! `ROUND_EXPIRY_SLOTS` gets every stake out of a round the oracle never
//! answered, and `stranded` in [`resolve_push`](super::resolve_push) closes a
//! star that can no longer be pushed. Neither covers the permanent case:
//!
//! * ORAO goes dark for good. Rounds still open and expire, so stakes keep
//!   coming back - but a refund adds no mass, so the star never dies, never
//!   reaches the hole cap, and never finishes. The pot paid in by every push
//!   that settled *before* the outage has no route out: no kill to pay a
//!   winner, no Event Horizon to pay the feeders. Feeders' SOL is stuck, and
//!   with no upgrade authority there is nothing to fix it with.
//! * The game simply dies. Nobody pushes again. Same shape, same stuck pot,
//!   and it does not even need anything to be broken.
//!
//! So: after [`STALL_SECS`] with no new mass - a day for a star still in its
//! nursery - anyone may collapse the star. Feeders claim through the usual
//! [`claim_hole_share`](super::claim_hole) path, and the pot pays out on one
//! rule:
//!
//! > feeders get back the pot's share of exactly what they fed; the remainder
//! > recycles into the next star's endowment.
//!
//! That rule is chosen so nobody has a reason to want a stall. A feeder cannot
//! profit from one, because a collapse returns their own stake at cost and
//! cancels the 21:1 hole ticket they were holding - it is strictly worse for
//! them than the star continuing, which is why the fuse can be long without
//! trapping anyone. The house cannot profit either: the residue is prize money,
//! and prize money is not withdrawable, so it can only ever fund the next star.
//! No lamport leaves the game and no lamport is created. `protocol_accrued` is
//! not touched at all.
//!
//! Nothing moves out of the vault here. The whole instruction is a reclassifying
//! of the star's pot between two liability buckets, plus a status change; the
//! actual transfers happen later, one feeder at a time, through
//! `claim_hole_share`.

use anchor_lang::prelude::*;

use crate::constants::{CONFIG_SEED, FEED_SEED, STAR_SEED};
use crate::errors::SoldustError;
use crate::events::StarStalled;
use crate::game_config;
use crate::math::{add, sub};
use crate::state::{Config, Star, StarFeed, StarStatus};

#[derive(Accounts)]
pub struct CollapseStalledStar<'info> {
    /// Anyone. Pays the fee and nothing else: no account is created here and no
    /// lamport leaves the vault, so there is nothing for a caller to gain or
    /// steer by being the one who submits it.
    pub cranker: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [STAR_SEED, &star.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    /// The star's feed tally, which decides how much of the pot the feeders can
    /// claim back.
    ///
    /// Required rather than optional, and that is safe: `feed` creates this
    /// account on the first nursery feed, so the only stars without one are
    /// stars nobody ever fed. Those hold nothing but their endowment - house
    /// money, recycled from the previous star - so there is no player SOL to
    /// free, and a single feed from anyone brings the account into existence.
    #[account(
        seeds = [FEED_SEED, &star.star_id.to_le_bytes()],
        bump = star_feed.bump,
    )]
    pub star_feed: Account<'info, StarFeed>,
}

pub fn collapse_stalled_star(ctx: Context<CollapseStalledStar>) -> Result<()> {
    let clock = Clock::get()?;
    let star = &ctx.accounts.star;

    require!(star.is_alive(), SoldustError::StarNotAlive);
    // Reported separately from the timeout because it is the one precondition the
    // caller can do something about: void the round with `expire_round`, then
    // resolve its members. Neither step needs the oracle, which is what makes a
    // collapse reachable in the exact scenario it exists for.
    require!(star.pending_pushes == 0, SoldustError::StarQueueNotEmpty);
    require!(
        star.is_stalled(clock.unix_timestamp),
        SoldustError::StarNotStalled
    );

    let early_volume = ctx.accounts.star_feed.early_volume;
    let prize = star.prize_pool;

    // What the feeders put in, less the rake their feeds already paid - the same
    // `split` every push goes through, so this is exactly the prize-side value
    // of their own money and not a lamport more.
    //
    // The `min` cannot bind: `early_volume` is a subset of `total_mass` and each
    // feed contributed its own prize share to `prize_pool`, with the per-feed
    // rounding running in the pot's favour. It is here so that this instruction
    // has no arithmetic failure mode at all - the one thing a liveness floor
    // must not have is a way to refuse.
    let claimable = game_config::economics().split(early_volume)?.0.min(prize);
    let recycled = sub(prize, claimable)?;

    let star_id = star.star_id;
    let star_key = star.key();
    let final_mass = star.total_mass;
    let successful_pushes = star.successful_pushes;
    let last_mass_ts = star.last_mass_ts;

    if recycled > 0 {
        let config = &mut ctx.accounts.config;
        config.next_star_reserve = add(config.next_star_reserve, recycled)?;
        config.prize_liability = sub(config.prize_liability, recycled)?;
    }

    let star = &mut ctx.accounts.star;
    star.status = StarStatus::Stalled;
    star.death_ts = clock.unix_timestamp;
    star.death_slot = clock.slot;
    // `claim_hole_share` divides this across the feeders. Randomness stays zero:
    // nothing was rolled here, and a reader can tell this from `status` alone.
    star.final_prize = claimable;

    emit!(StarStalled {
        star_id,
        star: star_key,
        final_mass,
        final_prize: claimable,
        recycled,
        early_volume,
        successful_pushes,
        last_mass_ts,
        death_ts: clock.unix_timestamp,
        death_slot: clock.slot,
    });
    Ok(())
}
