//! Early feeders collect their share of a collapsed star - either an Event
//! Horizon, where the pot is theirs in full, or a stall, where it is the
//! prize-side value of exactly what they fed.
//!
//! A share is `feed_share.amount * final_prize / early_volume`, floored. The
//! floor is deliberate and it always rounds *towards* the vault: the sum of every
//! share can fall a few lamports short of `final_prize`, never over it, so the
//! last claimant can never find the pot already empty. Those few lamports stay in
//! `prize_liability` and no path pays them out - unclaimable rather than lost, and
//! bounded by one lamport per feeder. Rounding the other way would mean a
//! shortfall paid out of somebody else's escrow, which is the failure worth
//! avoiding.

use anchor_lang::prelude::*;

use crate::constants::{
    CONFIG_SEED, FEED_SEED, FEED_SHARE_SEED, PLAYER_SEED, STAR_SEED, VAULT_SEED,
};
use crate::errors::SoldustError;
use crate::events::HoleShareClaimed;
use crate::math::{add, sub};
use crate::state::{Config, FeedShare, Player, Star, StarFeed, StarStatus};
use crate::vault;

#[derive(Accounts)]
pub struct ClaimHoleShare<'info> {
    #[account(mut)]
    pub player: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    #[account(
        mut,
        seeds = [STAR_SEED, &star.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    #[account(
        seeds = [FEED_SEED, &star.star_id.to_le_bytes()],
        bump = star_feed.bump,
    )]
    pub star_feed: Account<'info, StarFeed>,

    #[account(
        mut,
        seeds = [FEED_SHARE_SEED, &star.star_id.to_le_bytes(), player.key().as_ref()],
        bump = feed_share.bump,
        has_one = player @ SoldustError::NoFeedShare,
    )]
    pub feed_share: Account<'info, FeedShare>,

    #[account(
        mut,
        seeds = [PLAYER_SEED, player.key().as_ref()],
        bump = player_stats.bump,
    )]
    pub player_stats: Account<'info, Player>,

    pub system_program: Program<'info, System>,
}

pub fn claim_hole_share(ctx: Context<ClaimHoleShare>) -> Result<()> {
    // Both routes that pay feeders, and the split is identical either way:
    // `final_prize` over `early_volume`. Only the size of `final_prize` differs
    // - the whole pot at Event Horizon, the prize-side value of their own feeds
    // on a stall.
    require!(
        matches!(ctx.accounts.star.status, StarStatus::BlackHole | StarStatus::Stalled),
        SoldustError::StarNotBlackHole
    );
    require!(
        !ctx.accounts.feed_share.claimed,
        SoldustError::HoleShareAlreadyClaimed
    );
    require!(
        ctx.accounts.feed_share.amount > 0 && ctx.accounts.star_feed.early_volume > 0,
        SoldustError::NoFeedShare
    );

    let amount = ((ctx.accounts.feed_share.amount as u128)
        * (ctx.accounts.star.final_prize as u128)
        / (ctx.accounts.star_feed.early_volume as u128)) as u64;
    require!(amount > 0, SoldustError::NoFeedShare);

    let star_id = ctx.accounts.star.star_id;
    let vault_bump = ctx.accounts.config.vault_bump;

    ctx.accounts.feed_share.claimed = true;

    vault::pay(
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.player.to_account_info(),
        vault_bump,
        amount,
    )?;

    let config = &mut ctx.accounts.config;
    config.prize_liability = sub(config.prize_liability, amount)?;

    let stats = &mut ctx.accounts.player_stats;
    stats.prizes_won = add(stats.prizes_won, amount)?;

    emit!(HoleShareClaimed {
        star_id,
        player: ctx.accounts.player.key(),
        amount,
        claimed_ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}
