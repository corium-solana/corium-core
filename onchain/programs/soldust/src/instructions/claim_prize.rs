//! The Star Killer collects.
//!
//! Deliberately a separate, explicit action rather than an automatic payout:
//! claiming a jackpot is a positive intentional step, unlike a cancelled push
//! refund (which must never require the player to do anything).

use anchor_lang::prelude::*;

use crate::constants::{CONFIG_SEED, PLAYER_SEED, STAR_SEED, VAULT_SEED};
use crate::errors::SoldustError;
use crate::events::PrizeClaimed;
use crate::math::{add, sub};
use crate::state::{Config, Player, Star, StarStatus};
use crate::vault;

#[derive(Accounts)]
pub struct ClaimPrize<'info> {
    /// Checked against `star.killer` in the handler (the star account is
    /// deserialized after this one).
    #[account(mut)]
    pub winner: Signer<'info>,

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
        mut,
        seeds = [PLAYER_SEED, winner.key().as_ref()],
        bump = player_stats.bump,
    )]
    pub player_stats: Account<'info, Player>,

    pub system_program: Program<'info, System>,
}

pub fn claim_prize(ctx: Context<ClaimPrize>) -> Result<()> {
    require!(
        ctx.accounts.star.status == StarStatus::Dead,
        SoldustError::StarNotDead
    );
    require_keys_eq!(
        ctx.accounts.winner.key(),
        ctx.accounts.star.killer,
        SoldustError::NotStarKiller
    );
    require!(
        !ctx.accounts.star.prize_claimed,
        SoldustError::PrizeAlreadyClaimed
    );

    let amount = ctx.accounts.star.final_prize;
    let star_id = ctx.accounts.star.star_id;
    let vault_bump = ctx.accounts.config.vault_bump;

    // Latch first, pay second. Both live or die with the transaction, and a
    // failed transfer leaves `prize_claimed` false so the winner can retry.
    ctx.accounts.star.prize_claimed = true;

    vault::pay(
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.winner.to_account_info(),
        vault_bump,
        amount,
    )?;

    let config = &mut ctx.accounts.config;
    config.prize_liability = sub(config.prize_liability, amount)?;

    let stats = &mut ctx.accounts.player_stats;
    stats.prizes_won = add(stats.prizes_won, amount)?;

    emit!(PrizeClaimed {
        star_id,
        winner: ctx.accounts.winner.key(),
        amount,
        claimed_ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}
