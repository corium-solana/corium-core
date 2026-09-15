//! Star birth.
//!
//! Star #1 is created once, permissionlessly, using `Config.genesis_seed`.
//! Every star after that is also permissionless. The next one can be born
//! as soon as the current star is finished *or* has committed its full
//! mass (settled + queued ≥ hole cap), so play can move on while the old
//! queue still settles. The visual seed hashes the previous star's seed.

use anchor_lang::prelude::*;
use solana_sha256_hasher::hashv;

use crate::constants::{CONFIG_SEED, FEED_MASS, PUSH_STEP, STAR_SEED};
use crate::errors::SoldustError;
use crate::events::StarCreated;
use crate::math::add;
use crate::game_config::{self, LifecycleConfig};
use crate::state::{Config, Star, StarStatus};

/// A fresh star record. Everything else is zeroed.
fn new_star(
    star_id: u64,
    seed: [u8; 32],
    bump: u8,
    clock: &Clock,
    lifecycle: LifecycleConfig,
) -> Star {
    Star {
        star_id,
        seed,
        status: StarStatus::Alive,
        stage: 0,
        birth_ts: clock.unix_timestamp,
        birth_slot: clock.slot,
        death_ts: 0,
        death_slot: 0,
        successful_pushes: 0,
        pending_pushes: 0,
        pending_lamports: 0,
        cancelled_pushes: 0,
        push_counter: 0,
        current_round: 0,
        settle_cursor: 0,
        total_mass: 0,
        prize_pool: 0,
        lifecycle,
        killer: Pubkey::default(),
        killer_push: Pubkey::default(),
        killer_push_id: 0,
        final_prize: 0,
        prize_claimed: false,
        next_star_created: false,
        death_randomness: [0u8; 64],
        death_roll_ppb: 0,
        death_threshold_ppb: 0,
        bump,
        // Birth counts as the first sign of life, so the stall clock starts here
        // rather than at zero - which would make a newborn star collapsible on
        // its first slot.
        last_mass_ts: clock.unix_timestamp,
    }
}

// ---------------------------------------------------------------- first star

#[derive(Accounts)]
pub struct CreateFirstStar<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = payer,
        space = 8 + Star::INIT_SPACE,
        seeds = [STAR_SEED, &1u64.to_le_bytes()],
        bump,
    )]
    pub star: Account<'info, Star>,

    pub system_program: Program<'info, System>,
}

pub fn create_first_star(ctx: Context<CreateFirstStar>) -> Result<()> {
    // `init` on a fixed PDA already makes this once-only; the explicit check
    // just turns a raw "account in use" into a readable error.
    require!(
        ctx.accounts.config.current_star_id == 0,
        SoldustError::FirstStarAlreadyCreated
    );

    let clock = Clock::get()?;
    let seed = ctx.accounts.config.genesis_seed;
    let lifecycle = game_config::lifecycle();

    ctx.accounts
        .star
        .set_inner(new_star(1, seed, ctx.bumps.star, &clock, lifecycle));

    let config = &mut ctx.accounts.config;
    config.current_star_id = 1;
    config.stars_created = add(config.stars_created, 1)?;

    emit!(StarCreated {
        star_id: 1,
        star: ctx.accounts.star.key(),
        seed,
        derived_from_star_id: 0,
        birth_ts: clock.unix_timestamp,
        birth_slot: clock.slot,
        endowment: 0,
    });
    Ok(())
}

// ----------------------------------------------------------------- next star

#[derive(Accounts)]
#[instruction(new_star_id: u64)]
pub struct CreateNextStar<'info> {
    /// Permissionless. Pays rent for the new star account.
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [STAR_SEED, &config.current_star_id.to_le_bytes()],
        bump = prev_star.bump,
    )]
    pub prev_star: Account<'info, Star>,

    #[account(
        init,
        payer = payer,
        space = 8 + Star::INIT_SPACE,
        seeds = [STAR_SEED, &new_star_id.to_le_bytes()],
        bump,
    )]
    pub next_star: Account<'info, Star>,

    pub system_program: Program<'info, System>,
}

pub fn create_next_star(ctx: Context<CreateNextStar>, new_star_id: u64) -> Result<()> {
    let expected = add(ctx.accounts.config.current_star_id, 1)?;
    require!(new_star_id == expected, SoldustError::NotCurrentStar);
    require!(
        ctx.accounts.prev_star.is_closed(),
        SoldustError::StarStillOpen
    );
    // Belt and braces alongside `init`: a dead star can only ever father one
    // successor, even if the account were somehow closed and recreated.
    require!(
        !ctx.accounts.prev_star.next_star_created,
        SoldustError::NextStarAlreadyCreated
    );

    let clock = Clock::get()?;

    // Available as soon as the previous star exists. Death randomness is
    // not required - play can move on before the old star actually dies.
    let seed = hashv(&[
        b"soldust:star-seed",
        &ctx.accounts.prev_star.seed,
        &ctx.accounts.prev_star.star_id.to_le_bytes(),
        &new_star_id.to_le_bytes(),
    ])
    .to_bytes();

    // A recycled prize is `prize_bps` of something, so it rarely lands on the
    // push lattice. Endow whole steps only and leave the dust in the reserve
    // for the star after this one; mass has to stay a whole number of steps or
    // the room left to a boundary would stop being a legal push.
    //
    // Capped one step below the nursery so an endowed star always still has a
    // nursery to fill. That is what stops the recycle ratchet: a star born at or
    // above `FEED_MASS` rejects every `feed`, which guarantees `early_volume ==
    // 0`, which recycles its entire pot into the *next* endowment, which is
    // larger - and at `HOLE_MASS` a star would be born already full, unpushable,
    // uncollapsible, with its prize liability locked forever. With a nursery
    // there are always feeders, so the pot is always claimable and never
    // recycles. Whatever the cap leaves behind simply waits in the reserve.
    let endowment = game_config::floor_to_step(ctx.accounts.config.next_star_reserve)
        .min(FEED_MASS - PUSH_STEP);
    let lifecycle = game_config::lifecycle();

    let mut born = new_star(new_star_id, seed, ctx.bumps.next_star, &clock, lifecycle);
    if endowment > 0 {
        // Endowed mass carries no protocol cut - it has already paid one. So
        // `prize_pool` starts above `prize_bps` of mass, and every push at
        // this star returns a little better than fair until it is absorbed.
        born.total_mass = endowment;
        born.prize_pool = endowment;
        born.stage = lifecycle.stage_for_mass(endowment);
    }
    ctx.accounts.next_star.set_inner(born);

    ctx.accounts.prev_star.next_star_created = true;

    let config = &mut ctx.accounts.config;
    if endowment > 0 {
        config.prize_liability = add(config.prize_liability, endowment)?;
        config.next_star_reserve = crate::math::sub(config.next_star_reserve, endowment)?;
    }
    config.current_star_id = new_star_id;
    config.stars_created = add(config.stars_created, 1)?;

    emit!(StarCreated {
        star_id: new_star_id,
        star: ctx.accounts.next_star.key(),
        seed,
        derived_from_star_id: ctx.accounts.prev_star.star_id,
        birth_ts: clock.unix_timestamp,
        birth_slot: clock.slot,
        endowment,
    });
    Ok(())
}
