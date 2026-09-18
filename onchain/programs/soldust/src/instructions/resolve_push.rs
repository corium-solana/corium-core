//! Step 3 of a push: permissionless settlement.
//!
//! The player does not sign here. Anyone - our crank, another player, a
//! stranger - can submit this, and the outcome is identical either way
//! because everything that matters is read from accounts the caller cannot
//! forge: the push PDA, the star PDA and the round PDA. The draw is one of the
//! things read off the round - there is no oracle account in this instruction
//! at all, because the oracle wrote its answer onto the round before anyone got
//! here.
//!
//! Two paths:
//!
//! * **settle** - the star is alive and this push's round has a draw. The
//!   escrow settles into mass/prize/protocol, STARDUST is awarded, and the roll
//!   decides survival or supernova. The first lethal push wins.
//! * **refund** - the push never got to roll, because the star was already
//!   finished or because its round expired without a usable draw. Stake comes
//!   back in full in this same transaction. Randomness is not required on this
//!   path, which is exactly what makes a stuck oracle survivable.
//!
//! Both paths are strictly in `push_id` order (`push_id == star.settle_cursor`),
//! so a later request cannot steal the kill from an earlier one - and, just as
//! importantly, so a refund cannot step over a push that is still pending. The
//! cursor only ever increments, so a push the cursor passed could never resolve
//! again on either path; ordering the refund branch too is what keeps every
//! stake reachable. The queue always drains, because whichever push the cursor
//! points at can settle or be voided within `ROUND_EXPIRY_SLOTS`.
//!
//! Live settlement may still clip an oversize push, when the room left at
//! settled mass turns out lower than the room the queue quoted at request
//! time. The excess is refunded in the same transaction and the accepted
//! slice lands; the clip can never take the whole push, because `amount` is
//! at least one whole step and `accepted_settle_amount` never returns zero
//! for a live star (`settle_always_lands_something`). If the refund transfer
//! fails the whole transaction reverts and the push stays `Pending`, safely
//! retryable.
//!
//! The roll happens here rather than at request time, and that is what makes
//! a signed push immune to the queue: the threshold is the push's share of the
//! mass it actually creates, so a push that lands behind others gets a smaller
//! chance at a proportionally bigger pot and keeps the same expected value.
//! The *draw* it rolls against is shared with everyone else in its round, but
//! the roll is labelled with this push's own id, so its marginal probability is
//! still exactly `threshold_ppb` and no member's fate is tied to another's.

use anchor_lang::prelude::*;

use crate::constants::{
    CONFIG_SEED, FEED_MASS, FEED_SEED, FEED_SHARE_SEED, HOLE_MASS, PLAYER_SEED, PUSH_SEED,
    ROUND_SEED, STAR_SEED, VAULT_SEED,
};
use crate::errors::SoldustError;
use crate::events::{PushCancelled, PushResolved, StageChanged, StarCollapsed, StarDestroyed};
use crate::game_config;
use crate::math::{add, sub};
use crate::state::{
    Config, FeedShare, PendingPush, Player, PushStatus, Round, RoundStatus, Star, StarFeed,
    StarStatus,
};
use crate::{vault, vrf};

/// A star that is alive, has an empty queue, and is no longer the current star.
///
/// `request_push` only accepts the current star, so such a star can never take
/// another lamport of mass. It cannot nova (that needs a push) and it cannot
/// reach the hole cap on its own, so without intervention it stays Alive with a
/// pot nobody can ever claim. Reachable only via an expired round handing back
/// enough stake to drop committed mass below the cap after a successor was born.
fn stranded(star: &Star, config: &Config) -> bool {
    star.is_alive() && star.pending_pushes == 0 && star.star_id != config.current_star_id
}

/// Send a star to Event Horizon and settle its pot onto the feeders.
///
/// Called from two places, because there are two ways a star can run out of
/// road: it survived to the hole cap, or it can never be pushed again. See
/// `stranded` at the call site for the second one.
#[allow(clippy::too_many_arguments)]
fn collapse_into_hole(
    config: &mut Config,
    star: &mut Star,
    star_key: Pubkey,
    early_volume: u64,
    randomness: [u8; 64],
    roll_ppb: u32,
    threshold_ppb: u32,
    stranded: bool,
    clock: &Clock,
) -> Result<()> {
    let prize = star.prize_pool;
    // Feeders take the whole prize. If nobody fed, nobody can claim -
    // recycle that SOL into the next star so it does not sit in
    // prize_liability forever. (`echo` on the event is that recycle.)
    let (recycled, claimable) = if early_volume == 0 {
        (prize, 0)
    } else {
        (0, prize)
    };

    if recycled > 0 {
        config.next_star_reserve = add(config.next_star_reserve, recycled)?;
        config.prize_liability = sub(config.prize_liability, recycled)?;
    }

    star.status = StarStatus::BlackHole;
    star.death_ts = clock.unix_timestamp;
    star.death_slot = clock.slot;
    star.final_prize = claimable;
    star.death_randomness = randomness;
    star.death_roll_ppb = roll_ppb;
    star.death_threshold_ppb = threshold_ppb;

    emit!(StarCollapsed {
        star_id: star.star_id,
        star: star_key,
        final_mass: star.total_mass,
        final_prize: claimable,
        echo: recycled,
        early_volume,
        successful_pushes: star.successful_pushes,
        stranded,
        death_ts: clock.unix_timestamp,
        death_slot: clock.slot,
    });
    Ok(())
}

/// Boxed for the same reason as [`RequestPush`](super::request_push::RequestPush):
/// seven deserialized accounts in one `try_accounts` is a 4608-byte stack frame
/// against a 4096-byte limit. Do not unbox them.
#[derive(Accounts)]
pub struct ResolvePush<'info> {
    /// Anyone. Pays the transaction fee and nothing else - every account this
    /// instruction touches already exists.
    pub resolver: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    #[account(
        mut,
        seeds = [PUSH_SEED, pending_push.player.as_ref(), pending_push.client_seed.as_ref()],
        bump = pending_push.bump,
    )]
    pub pending_push: Box<Account<'info, PendingPush>>,

    /// Pinned by seed to the star the push was aimed at, so a draw can never be
    /// resolved against a different star.
    #[account(
        mut,
        seeds = [STAR_SEED, &pending_push.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Box<Account<'info, Star>>,

    /// Pinned by seed to the round this push joined, so it can only ever settle
    /// against the draw it committed to before that draw's seed existed.
    #[account(
        seeds = [ROUND_SEED, &pending_push.star_id.to_le_bytes(), &pending_push.round_id.to_le_bytes()],
        bump = round.bump,
    )]
    pub round: Box<Account<'info, Round>>,

    #[account(
        mut,
        seeds = [PLAYER_SEED, pending_push.player.as_ref()],
        bump = player_stats.bump,
    )]
    pub player_stats: Box<Account<'info, Player>>,

    /// CHECK: refund destination, pinned to the original pusher and nothing
    /// else. Deliberately *not* `SystemAccount`: that would additionally
    /// require the wallet to still be owned by the system program, and a
    /// player who assigns their account to some other program - which the
    /// system program lets them do with their own signature, irreversibly -
    /// would have their stake locked in the vault forever with no instruction
    /// able to release it. A system transfer only constrains its source, so
    /// paying out to a non-system account is perfectly legal.
    #[account(mut, address = pending_push.player @ SoldustError::PushPlayerMismatch)]
    pub player_wallet: UncheckedAccount<'info>,

    /// Both of these were created and paid for by `request_push`, so the
    /// resolver never funds an account here.
    #[account(
        mut,
        seeds = [FEED_SEED, &pending_push.star_id.to_le_bytes()],
        bump = star_feed.bump,
    )]
    pub star_feed: Box<Account<'info, StarFeed>>,

    #[account(
        mut,
        seeds = [FEED_SHARE_SEED, &pending_push.star_id.to_le_bytes(), pending_push.player.as_ref()],
        bump = feed_share.bump,
    )]
    pub feed_share: Box<Account<'info, FeedShare>>,

    pub system_program: Program<'info, System>,
}

pub fn resolve_push(ctx: Context<ResolvePush>) -> Result<()> {
    let clock = Clock::get()?;

    require!(
        ctx.accounts.pending_push.is_pending(),
        SoldustError::PushAlreadyResolved
    );
    require!(
        ctx.accounts.star.star_id == ctx.accounts.pending_push.star_id,
        SoldustError::PushStarMismatch
    );
    require!(
        ctx.accounts.round.round_id == ctx.accounts.pending_push.round_id
            && ctx.accounts.round.star_id == ctx.accounts.pending_push.star_id,
        SoldustError::PushRoundMismatch
    );

    let amount = ctx.accounts.pending_push.amount;
    let star_id = ctx.accounts.pending_push.star_id;
    let push_id = ctx.accounts.pending_push.push_id;
    let round_id = ctx.accounts.pending_push.round_id;
    let player = ctx.accounts.pending_push.player;
    let push_key = ctx.accounts.pending_push.key();

    let vault_bump = ctx.accounts.config.vault_bump;
    let economics = game_config::economics();
    let lifecycle = ctx.accounts.star.lifecycle;

    // Request order is the win order, and it binds *both* paths.
    //
    // `settle_cursor` is the next `push_id` that may resolve, and every writer
    // of it only ever increments. So this check has to gate the refund branch
    // too: a refund that skipped the queue would advance the cursor past a push
    // that is still pending, and that push could then never satisfy the check
    // again. Its only other exit is this same refund branch, which needs either
    // a finished star or an expired round - and a round whose draw has already
    // landed can never expire. The stake would have no exit at all.
    //
    // Ordering costs nothing here, because the push the cursor points at can
    // always resolve one way or the other within `ROUND_EXPIRY_SLOTS`: its round
    // either has a draw to settle against, or becomes voidable.
    require!(
        push_id == ctx.accounts.star.settle_cursor,
        SoldustError::PushOutOfOrder
    );

    // ---------------------------------------------------- refund path
    // Two reasons a push never rolls: the star finished before it got there, or
    // its round was voided. Neither needs randomness, which is what makes both
    // recoverable when the oracle is the thing that broke.
    let voided = ctx.accounts.round.status == RoundStatus::Expired;
    if ctx.accounts.star.is_finished() || voided {
        vault::pay(
            &ctx.accounts.system_program.to_account_info(),
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.player_wallet.to_account_info(),
            vault_bump,
            amount,
        )?;

        {
            let config = &mut ctx.accounts.config;
            config.pending_liability = sub(config.pending_liability, amount)?;
        }
        {
            let star = &mut ctx.accounts.star;
            star.pending_pushes = sub(star.pending_pushes, 1)?;
            star.pending_lamports = sub(star.pending_lamports, amount)?;
            star.cancelled_pushes = add(star.cancelled_pushes, 1)?;
            // Advance the cursor even here, or a voided batch would wedge every
            // push queued behind it for the rest of the star's life.
            star.settle_cursor = add(star.settle_cursor, 1)?;
        }

        // A voided round can hand back enough stake to drop a star back under
        // the hole cap *after* a successor was already born, and a star that is
        // not the current one can never be pushed again. Left alone it would
        // stay Alive at that mass forever with its pot stuck in
        // `prize_liability` and no instruction able to release it. So the refund
        // that drains the last of its queue finishes it instead, into the hole -
        // which is exactly the outcome its feeders bought a ticket on.
        if stranded(&ctx.accounts.star, &ctx.accounts.config) {
            let early_volume = ctx.accounts.star_feed.early_volume;
            let star_key = ctx.accounts.star.key();
            collapse_into_hole(
                &mut ctx.accounts.config,
                &mut ctx.accounts.star,
                star_key,
                early_volume,
                [0u8; 64],
                0,
                0,
                true,
                &clock,
            )?;
        }

        let stats = &mut ctx.accounts.player_stats;
        stats.cancelled_pushes = add(stats.cancelled_pushes, 1)?;

        let push = &mut ctx.accounts.pending_push;
        push.status = PushStatus::Cancelled;
        push.resolved_ts = clock.unix_timestamp;
        push.resolved_slot = clock.slot;

        emit!(PushCancelled {
            star_id,
            push_id,
            round_id,
            push: push_key,
            player,
            amount,
            round_voided: voided,
            resolved_ts: clock.unix_timestamp,
            resolved_slot: clock.slot,
        });
        return Ok(());
    }

    // ------------------------------------------------ live star: settle path
    // Only a draw this program asked for may be read, and `Drawn` is the only
    // status that says one arrived. An `Open` round was never even sealed; a
    // `Requested` one is still waiting on the oracle. Neither has a draw.
    //
    // The two are told apart so the caller learns which it is: "not sealed yet"
    // is a crank ordering mistake, "not answered yet" is just a retry.
    require!(
        ctx.accounts.round.status != RoundStatus::Open,
        SoldustError::RoundNotRequested
    );
    require!(
        ctx.accounts.round.status == RoundStatus::Drawn,
        SoldustError::RandomnessNotReady
    );

    let mass_before = ctx.accounts.star.total_mass;
    let accepted = economics.accepted_settle_amount(amount, mass_before);

    // Widened to the 64-byte shape the roll and the death record are defined
    // over, so the math is bit-for-bit what it was under ORAO.
    let randomness = vrf::widen(&ctx.accounts.round.randomness);

    let excess = amount - accepted;
    if excess > 0 {
        vault::pay(
            &ctx.accounts.system_program.to_account_info(),
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.player_wallet.to_account_info(),
            vault_bump,
            excess,
        )?;
        {
            let config = &mut ctx.accounts.config;
            config.pending_liability = sub(config.pending_liability, excess)?;
        }
        {
            let star = &mut ctx.accounts.star;
            star.pending_lamports = sub(star.pending_lamports, excess)?;
        }
        ctx.accounts.pending_push.amount = accepted;
    }

    let amount = accepted;
    let (prize_cut, protocol_cut) = economics.split(amount)?;
    // The whole odds model: this push's share of the mass it creates. Nothing
    // is read from a table, and `prize_pool` is `prize_bps` of that same mass,
    // so the payout it is rolling for is exactly `1 / threshold` times the
    // stake, less the rake.
    let threshold_ppb = if mass_before < FEED_MASS {
        // Game A: below 1 SOL the push is a hole ticket. No supernova.
        0
    } else {
        game_config::nova_ppb(amount, mass_before.saturating_add(amount))
    };
    let previous_stage = ctx.accounts.star.stage;
    let stage_mult = ctx.accounts.pending_push.stardust_mult_bps;
    let stardust = economics.stardust_for(
        amount,
        stage_mult,
        game_config::nova_ppb(amount, mass_before.saturating_add(amount)),
    )?;

    {
        let config = &mut ctx.accounts.config;
        config.pending_liability = sub(config.pending_liability, amount)?;
        config.protocol_accrued = add(config.protocol_accrued, protocol_cut)?;
        config.prize_liability = add(config.prize_liability, prize_cut)?;
        config.total_pushes_settled = add(config.total_pushes_settled, 1)?;
        config.total_volume = add(config.total_volume, amount)?;
    }
    let star_mass;
    let star_prize_pool;
    {
        let star = &mut ctx.accounts.star;
        star.total_mass = add(star.total_mass, amount)?;
        star.prize_pool = add(star.prize_pool, prize_cut)?;
        star.successful_pushes = add(star.successful_pushes, 1)?;
        star.settle_cursor = add(star.settle_cursor, 1)?;
        star.pending_pushes = sub(star.pending_pushes, 1)?;
        star.pending_lamports = sub(star.pending_lamports, amount)?;
        star.stage = lifecycle.stage_for_mass(star.total_mass);
        // Only this branch stamps liveness. A refund also reaches `resolve_push`
        // and also moves the star, but it lands no mass and needs no draw, so a
        // star that can *only* refund is exactly the stall this guards against.
        // See `Star::is_stalled`.
        star.last_mass_ts = clock.unix_timestamp;
        star_mass = star.total_mass;
        star_prize_pool = star.prize_pool;
    }

    if mass_before < FEED_MASS {
        let feed = &mut ctx.accounts.star_feed;
        feed.early_volume = add(feed.early_volume, amount)?;
        let share = &mut ctx.accounts.feed_share;
        share.amount = add(share.amount, amount)?;
    }

    {
        let stats = &mut ctx.accounts.player_stats;
        stats.successful_pushes = add(stats.successful_pushes, 1)?;
        stats.total_sol_pushed = add(stats.total_sol_pushed, amount)?;
        stats.stardust = add(stats.stardust, stardust)?;
    }

    let new_stage = ctx.accounts.star.stage;
    if new_stage != previous_stage {
        emit!(StageChanged {
            star_id,
            from_stage: previous_stage,
            to_stage: new_stage,
            star_mass,
        });
    }

    // One draw, one roll per member. The `push_id` label is what keeps the
    // members of a round independent of each other.
    let roll_ppb = vrf::roll_for(&randomness, push_id);
    let lethal = threshold_ppb > 0 && roll_ppb < threshold_ppb;

    {
        let push = &mut ctx.accounts.pending_push;
        push.roll_ppb = roll_ppb;
        push.threshold_ppb = threshold_ppb;
        push.resolved_ts = clock.unix_timestamp;
        push.resolved_slot = clock.slot;
        push.status = if lethal {
            PushStatus::Killed
        } else {
            PushStatus::Survived
        };
    }

    if lethal {
        {
            let star = &mut ctx.accounts.star;
            star.status = StarStatus::Dead;
            star.death_ts = clock.unix_timestamp;
            star.death_slot = clock.slot;
            star.killer = player;
            star.killer_push = push_key;
            star.killer_push_id = push_id;
            // Locked in: every later push against this star is refunded, so
            // the pool cannot move again. That includes the rest of this same
            // round - a draw that kills partway through simply stops being
            // read, and the members behind the kill take the refund path.
            star.final_prize = star.prize_pool;
            star.death_randomness = randomness;
            star.death_roll_ppb = roll_ppb;
            star.death_threshold_ppb = threshold_ppb;
        }

        let stats = &mut ctx.accounts.player_stats;
        stats.stars_killed = add(stats.stars_killed, 1)?;

        emit!(StarDestroyed {
            star_id,
            star: ctx.accounts.star.key(),
            killer: player,
            killer_push: push_key,
            killer_push_id: push_id,
            killer_round_id: round_id,
            final_mass: star_mass,
            final_prize: star_prize_pool,
            successful_pushes: ctx.accounts.star.successful_pushes,
            roll_ppb,
            threshold_ppb,
            randomness,
            death_ts: clock.unix_timestamp,
            death_slot: clock.slot,
        });
    } else {
        // Survived to the cap, or survived into a dead end: an earlier voided
        // round can leave a star that has a successor unable to ever reach the
        // cap, and the settle that empties its queue is the last chance to
        // resolve its pot. See `stranded`.
        let at_cap = star_mass >= HOLE_MASS;
        let dead_end = stranded(&ctx.accounts.star, &ctx.accounts.config);
        if at_cap || dead_end {
            let early_volume = ctx.accounts.star_feed.early_volume;
            let star_key = ctx.accounts.star.key();
            collapse_into_hole(
                &mut ctx.accounts.config,
                &mut ctx.accounts.star,
                star_key,
                early_volume,
                randomness,
                roll_ppb,
                threshold_ppb,
                !at_cap,
                &clock,
            )?;
        }
    }

    emit!(PushResolved {
        star_id,
        push_id,
        round_id,
        push: push_key,
        player,
        amount,
        stake_refund: excess,
        survived: !lethal,
        roll_ppb,
        threshold_ppb,
        star_mass,
        star_prize_pool,
        stardust_awarded: stardust,
        resolved_ts: clock.unix_timestamp,
        resolved_slot: clock.slot,
    });
    Ok(())
}

// ------------------------------------------------------------- rent recovery

/// Optional cleanup: return the push account's rent to the player once the
/// push has reached a terminal state. Purely a convenience - the lifecycle
/// history lives in events, not in these accounts.
///
/// Unlike the old per-push-seed design, closing this is now completely safe to
/// re-request under: `client_seed` no longer picks a VRF seed, it picks this
/// address and - for a push that opens a round - that round's entropy, while a
/// push's roll comes from its round plus its `push_id`, which is drawn from a
/// counter that never repeats.
#[derive(Accounts)]
pub struct ClosePush<'info> {
    #[account(
        mut,
        close = player_wallet,
        seeds = [PUSH_SEED, pending_push.player.as_ref(), pending_push.client_seed.as_ref()],
        bump = pending_push.bump,
    )]
    pub pending_push: Account<'info, PendingPush>,

    /// CHECK: rent goes back to whoever paid it, i.e. the original pusher, and
    /// only there. Not `SystemAccount` for the same reason as the refund
    /// destination above - see `ResolvePush::player_wallet`.
    #[account(mut, address = pending_push.player @ SoldustError::PushPlayerMismatch)]
    pub player_wallet: UncheckedAccount<'info>,
}

pub fn close_push(ctx: Context<ClosePush>) -> Result<()> {
    require!(
        !ctx.accounts.pending_push.is_pending(),
        SoldustError::PushStillPending
    );
    Ok(())
}
