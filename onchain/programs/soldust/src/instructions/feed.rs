//! Nursery feed: one signature, instant settle, no VRF.
//!
//! A hole ticket is just SOL in and a share written. There is nothing to
//! roll, so no draw is ever requested for it. Oversize clips to leftover room;
//! a full nursery fails before any transfer.
//!
//! Feeding cannot kill the star, which is what keeps the two books apart: a
//! feeder's stake buys a share of the hole at fair 21:1 odds, and a last hit
//! buys a nova chance instead. If nursery money could do both it would be
//! taking more than `prize_bps` back.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

use crate::constants::{
    CONFIG_SEED, FEED_MASS, FEED_SEED, FEED_SHARE_SEED, PLAYER_SEED, STAR_SEED, VAULT_SEED,
};
use crate::errors::SoldustError;
use crate::events::{PushResolved, StageChanged};
use crate::game_config::{self, Economics};
use crate::math::add;
use crate::state::{Config, FeedShare, Player, Star, StarFeed};

#[derive(Accounts)]
#[instruction(star_id: u64)]
pub struct Feed<'info> {
    #[account(mut)]
    pub player: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    #[account(
        mut,
        seeds = [STAR_SEED, &star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + Player::INIT_SPACE,
        seeds = [PLAYER_SEED, player.key().as_ref()],
        bump,
    )]
    pub player_stats: Account<'info, Player>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + StarFeed::INIT_SPACE,
        seeds = [FEED_SEED, &star_id.to_le_bytes()],
        bump,
    )]
    pub star_feed: Account<'info, StarFeed>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + FeedShare::INIT_SPACE,
        seeds = [FEED_SHARE_SEED, &star_id.to_le_bytes(), player.key().as_ref()],
        bump,
    )]
    pub feed_share: Account<'info, FeedShare>,

    pub system_program: Program<'info, System>,
}

pub fn feed(ctx: Context<Feed>, star_id: u64, mut amount: u64) -> Result<()> {
    let clock = Clock::get()?;
    let player_key = ctx.accounts.player.key();

    require!(
        ctx.accounts.config.current_star_id == star_id,
        SoldustError::NotCurrentStar
    );
    require!(
        ctx.accounts.star.star_id == star_id,
        SoldustError::PushStarMismatch
    );
    require!(ctx.accounts.star.is_alive(), SoldustError::StarNotAlive);

    let mass_before = ctx.accounts.star.total_mass;
    require!(mass_before < FEED_MASS, SoldustError::FeedWindowClosed);

    let economics = crate::game_config::economics();
    let room = Economics::room_at(mass_before);
    require!(room > 0, SoldustError::FeedWindowClosed);
    amount = Economics::accept_push_amount(amount, room)?;

    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            Transfer {
                from: ctx.accounts.player.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        ),
        amount,
    )?;

    let lifecycle = ctx.accounts.star.lifecycle;
    let (prize_cut, protocol_cut) = economics.split(amount)?;
    let stage_mult = lifecycle.stardust_mult_bps_at(mass_before);
    // A feed never rolls, but its share of the mass it creates is still what
    // sizes the STARDUST sweetener, so a feeder and a pusher of the same
    // relative size mint the same.
    let chance_ppb = game_config::nova_ppb(amount, mass_before + amount);
    let stardust = economics.stardust_for(amount, stage_mult, chance_ppb)?;
    let previous_stage = ctx.accounts.star.stage;
    let push_id = ctx.accounts.star.push_counter;

    {
        let config = &mut ctx.accounts.config;
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
        star.push_counter = add(star.push_counter, 1)?;
        star.successful_pushes = add(star.successful_pushes, 1)?;
        // A feed consumes a `push_id` and settles inside this transaction, so
        // it has to move the settle cursor with it. Skip this and the cursor
        // falls behind `push_counter` by one per feed, and every last-hit after
        // the nursery is permanently `PushOutOfOrder`.
        star.settle_cursor = add(star.settle_cursor, 1)?;
        star.stage = lifecycle.stage_for_mass(star.total_mass);
        // The star gained mass, so it is demonstrably alive: reset the stall
        // clock. See `Star::is_stalled`.
        star.last_mass_ts = clock.unix_timestamp;
        star_mass = star.total_mass;
        star_prize_pool = star.prize_pool;
    }

    {
        let feed = &mut ctx.accounts.star_feed;
        if feed.star_id == 0 {
            feed.star_id = star_id;
            feed.bump = ctx.bumps.star_feed;
        }
        feed.early_volume = add(feed.early_volume, amount)?;
        let share = &mut ctx.accounts.feed_share;
        if share.player == Pubkey::default() {
            share.star_id = star_id;
            share.player = player_key;
            share.bump = ctx.bumps.feed_share;
        }
        share.amount = add(share.amount, amount)?;
    }

    {
        let stats = &mut ctx.accounts.player_stats;
        if stats.wallet == Pubkey::default() {
            stats.wallet = player_key;
            stats.bump = ctx.bumps.player_stats;
        }
        stats.requested_pushes = add(stats.requested_pushes, 1)?;
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

    let (push_key, _) = Pubkey::find_program_address(
        &[b"feed-hit", &star_id.to_le_bytes(), &push_id.to_le_bytes()],
        ctx.program_id,
    );

    emit!(PushResolved {
        star_id,
        push_id,
        // A feed never rolls, so it joins no round and no draw is ever bought
        // on its behalf. `u64::MAX` reads as "not a member of anything".
        round_id: u64::MAX,
        push: push_key,
        player: player_key,
        amount,
        // A feed settles instantly, so there is nothing to hand back.
        stake_refund: 0,
        survived: true,
        roll_ppb: 0,
        threshold_ppb: 0,
        star_mass,
        star_prize_pool,
        stardust_awarded: stardust,
        resolved_ts: clock.unix_timestamp,
        resolved_slot: clock.slot,
    });
    Ok(())
}
