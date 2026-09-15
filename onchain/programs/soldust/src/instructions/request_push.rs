//! Step 1 of a last-hit: the only transaction the player signs.
//!
//! Escrows the stake, records the intent against *this exact star*, and joins
//! the star's currently open round. The player pays nothing for randomness: the
//! draw is bought once per round out of the house's own rake, so a busy star
//! costs the protocol a fraction of a request per push and the player sees a
//! flat `prize_bps` return with no oracle line item at all.
//!
//! Joining a round is what commits the player before the draw exists. The
//! round's ORAO seed is not decidable until `close_round` seals it and samples
//! a slot hash, so at the moment this signature lands there is nothing to
//! grind - not by the player, and not by anyone watching the mempool, because
//! there is no per-push seed here for a bystander to burn.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

use crate::constants::{
    CONFIG_SEED, FEED_MASS, FEED_SEED, FEED_SHARE_SEED, HOLE_MASS, PLAYER_SEED, PUSH_SEED,
    ROUND_SEED, STAR_SEED, VAULT_SEED,
};
use crate::errors::SoldustError;
use crate::events::PushRequested;
use crate::game_config::{self, Economics};
use crate::math::add;
use crate::state::{
    Config, FeedShare, PendingPush, Player, PushStatus, Round, RoundStatus, Star, StarFeed,
};
use crate::vrf;

/// Every deserialized account here is boxed, and has to stay boxed.
///
/// This struct binds seven accounts, one of them `Star` at 380 bytes, and
/// Anchor generates a single `try_accounts` that holds all of them at once. On
/// the stack that frame is 4672 bytes against a hard BPF limit of 4096: the
/// linker reports `overflows the maximum allowed frame space`, and the binary it
/// still emits corrupts account resolution, so every push fails
/// `ConstraintSeeds` on `pending_push`. Boxing moves the payloads to the heap
/// and leaves pointers in the frame.
///
/// It is not cosmetic and it is not toolchain trivia: whether a frame lands
/// under 4096 is a codegen detail that moves between platform-tools releases, so
/// unboxing this compiles fine on one and ships undefined behaviour on the next.
/// `resolve_push` has the same shape for the same reason.
#[derive(Accounts)]
#[instruction(star_id: u64, amount: u64, client_seed: [u8; 32])]
pub struct RequestPush<'info> {
    #[account(mut)]
    pub player: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    #[account(
        mut,
        seeds = [STAR_SEED, &star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Box<Account<'info, Star>>,

    /// The star's open round. Created by whoever gets there first and shared by
    /// everyone who arrives before it is sealed.
    ///
    /// Pinned to `star.current_round` by seed, so a player cannot pick which
    /// round to join - in particular they cannot re-join a round whose seed is
    /// already fixed, because `draw_round` bumps this counter as it seals.
    ///
    /// Its rent is charged to whoever opens it and returned by
    /// `close_round_account` once the batch has fully resolved.
    #[account(
        init_if_needed,
        payer = player,
        space = 8 + Round::INIT_SPACE,
        seeds = [ROUND_SEED, &star_id.to_le_bytes(), &star.current_round.to_le_bytes()],
        bump,
    )]
    pub round: Box<Account<'info, Round>>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + Player::INIT_SPACE,
        seeds = [PLAYER_SEED, player.key().as_ref()],
        bump,
    )]
    pub player_stats: Box<Account<'info, Player>>,

    /// Scoped to the player so two people pushing in the same slot can never
    /// collide on this address, and `init` stops one player double-spending a
    /// `client_seed` that is still in flight. Reuse after `close_push` is
    /// harmless now: `client_seed` feeds nothing but this address and, for a push
    /// that opens a round, that round's entropy - and `push_id` comes from the
    /// star's counter, which never repeats, so neither can be replayed.
    #[account(
        init,
        payer = player,
        space = 8 + PendingPush::INIT_SPACE,
        seeds = [PUSH_SEED, player.key().as_ref(), client_seed.as_ref()],
        bump,
    )]
    pub pending_push: Box<Account<'info, PendingPush>>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + StarFeed::INIT_SPACE,
        seeds = [FEED_SEED, &star_id.to_le_bytes()],
        bump,
    )]
    pub star_feed: Box<Account<'info, StarFeed>>,

    #[account(
        init_if_needed,
        payer = player,
        space = 8 + FeedShare::INIT_SPACE,
        seeds = [FEED_SHARE_SEED, &star_id.to_le_bytes(), player.key().as_ref()],
        bump,
    )]
    pub feed_share: Box<Account<'info, FeedShare>>,

    pub system_program: Program<'info, System>,
}

pub fn request_push(
    ctx: Context<RequestPush>,
    star_id: u64,
    mut amount: u64,
    client_seed: [u8; 32],
) -> Result<()> {
    let clock = Clock::get()?;
    let player_key = ctx.accounts.player.key();

    {
        let config = &ctx.accounts.config;
        require!(
            config.current_star_id == star_id,
            SoldustError::NotCurrentStar
        );
        let settled = ctx.accounts.star.total_mass;
        require!(settled >= FEED_MASS, SoldustError::LastHitNotOpen);
        require!(
            ctx.accounts.star.committed_mass() < HOLE_MASS,
            SoldustError::StarClosed
        );
        // Room is quoted against committed mass so the queue cannot promise
        // the same lamports twice. Whatever the queue has since taken is
        // clipped at settle and refunded there.
        let room = Economics::room_at(ctx.accounts.star.committed_mass());
        amount = Economics::accept_push_amount(amount, room)?;
    }
    require!(
        ctx.accounts.star.star_id == star_id,
        SoldustError::PushStarMismatch
    );
    require!(ctx.accounts.star.is_alive(), SoldustError::StarNotAlive);

    let round_id = ctx.accounts.star.current_round;
    let push_id = ctx.accounts.star.push_counter;

    // Stake only. Nothing here pays for randomness.
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

    let quote_mass = ctx.accounts.star.committed_mass();
    let stardust_mult_bps = ctx.accounts.star.lifecycle.stardust_mult_bps_at(quote_mass);
    // Display only. The roll uses settled mass at resolve, which may be lower
    // if the queue ahead of this push shrinks, or higher if it grows - and
    // either way the push's expected value is unchanged, because the pot it
    // is shooting at moves by the same factor.
    let chance_ppb = game_config::nova_ppb(amount, quote_mass.saturating_add(amount));

    {
        let round = &mut ctx.accounts.round;
        if round.member_count == 0 {
            // First member opens the window. Measuring it from here rather than
            // from some global tick means a lone pusher on a quiet star waits
            // out one window, not the tail of somebody else's.
            round.star_id = star_id;
            round.round_id = round_id;
            round.first_push_id = push_id;
            round.status = RoundStatus::Open;
            round.opened_slot = clock.slot;
            round.opened_ts = clock.unix_timestamp;
            round.bump = ctx.bumps.round;
            // Whose wallet just paid for this account, so `close_round_account`
            // can give it back. Being first into a batch is luck, not a service
            // anyone bought, so it should not cost more than arriving second.
            round.opened_by = player_key;
            // And the player-supplied half of the seed, committed once for the
            // whole round.
            //
            // This deliberately does not accumulate over later members. The ORAO
            // account address is a function of the seed, Solana needs every
            // address before execution, and `draw_round` recomputes the seed from
            // this field - so anything that moves it invalidates a draw that is
            // already in flight. Folding every arrival in meant a member could
            // revert the crank's transaction just by turning up, which on a busy
            // star is most of them: the round would keep growing, every attempt
            // would keep missing, and the batch could run all the way to
            // `ROUND_EXPIRY_SLOTS` and refund without ever taking a roll.
            //
            // One member's contribution does the whole job this input has, which
            // is to stop the draw-time slot hash being the only ingredient - so
            // that neither a member nor a block leader decides a seed alone. It
            // is committed here, before the slot hash it will be mixed with even
            // exists. Later members lose nothing by not being in it: their roll
            // is labelled with their own `push_id`, and no amount of seed
            // material makes a VRF output they can predict.
            round.entropy = vrf::fold_entropy(&round.entropy, &client_seed, &player_key, push_id);
        }
        // Belt and braces alongside the seed pin: only an open round takes
        // members, so a sealed seed can never acquire a new one.
        require!(round.is_open(), SoldustError::RoundNotOpen);
        require!(round.star_id == star_id, SoldustError::PushStarMismatch);

        round.member_count = add(round.member_count, 1)?;
        // What lets `draw_round` know whether this batch has paid for its own
        // draw yet. The accepted amount, so it matches what will actually rake.
        round.stake = add(round.stake, amount)?;
    }
    {
        let star = &mut ctx.accounts.star;
        star.push_counter = add(star.push_counter, 1)?;
        star.pending_pushes = add(star.pending_pushes, 1)?;
        star.pending_lamports = add(star.pending_lamports, amount)?;
    }
    {
        let config = &mut ctx.accounts.config;
        config.pending_liability = add(config.pending_liability, amount)?;
    }
    {
        let stats = &mut ctx.accounts.player_stats;
        if stats.wallet == Pubkey::default() {
            stats.wallet = player_key;
            stats.bump = ctx.bumps.player_stats;
        }
        stats.requested_pushes = add(stats.requested_pushes, 1)?;
    }
    {
        let feed = &mut ctx.accounts.star_feed;
        if feed.star_id == 0 {
            feed.star_id = star_id;
            feed.bump = ctx.bumps.star_feed;
        }
        let share = &mut ctx.accounts.feed_share;
        if share.player == Pubkey::default() {
            share.star_id = star_id;
            share.player = player_key;
            share.bump = ctx.bumps.feed_share;
        }
    }

    ctx.accounts.pending_push.set_inner(PendingPush {
        star_id,
        push_id,
        round_id,
        player: player_key,
        amount,
        status: PushStatus::Pending,
        requested_ts: clock.unix_timestamp,
        requested_slot: clock.slot,
        resolved_ts: 0,
        resolved_slot: 0,
        roll_ppb: 0,
        threshold_ppb: 0,
        chance_ppb,
        stardust_mult_bps,
        client_seed,
        bump: ctx.bumps.pending_push,
    });

    emit!(PushRequested {
        star_id,
        push_id,
        round_id,
        push: ctx.accounts.pending_push.key(),
        round: ctx.accounts.round.key(),
        player: player_key,
        amount,
        client_seed,
        round_members: ctx.accounts.round.member_count,
        requested_ts: clock.unix_timestamp,
        requested_slot: clock.slot,
        chance_ppb,
    });
    Ok(())
}
