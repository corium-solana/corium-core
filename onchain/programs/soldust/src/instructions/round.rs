//! The round lifecycle: draw, void, or sweep.
//!
//! All of it is permissionless. A round moves `Open -> Requested`, or sideways to
//! `Expired` if it stalls, and never backwards.
//!
//! ```text
//!   request_push ...              members join, stake accumulates, seed fixed
//!        |
//!        |  draw_round         window elapsed or target size reached, AND the
//!        |                     batch's rake covers ORAO's current price.
//!        v                     seed = H(entropy, slot_hash, cranker) and the
//!    Requested                 one ORAO request for the batch, same transaction
//!        |
//!        |  ORAO fulfills off-chain
//!        v
//!     resolve_push x N         each member rolls H(draw, push_id)
//!        |
//!        |  close_round_account once every member has resolved
//!        v
//!     (rent back to the member who opened it)
//! ```
//!
//! ## Why sealing and drawing are one transaction
//!
//! They used to be two, on the theory that publishing the seed before anyone paid
//! for it removed all discretion from the purchase. The discretion argument was
//! right and the split was still wrong, because it published the seed while the
//! ORAO account that seed derives was still unoccupied - and that account is
//! first-come, first-served at ORAO. Anyone could read the sealed seed, request it
//! themselves for the price of one ORAO request, and leave the round permanently
//! undrawable: the program refuses to adopt a randomness account it did not
//! create (correctly - adopting one would let a griefer hand a round a draw they
//! had already read), so the round could only ever expire. Cheap, repeatable, and
//! it made a star unplayable for as long as the griefer kept paying.
//!
//! Deciding the seed inside the transaction that spends it closes that window
//! completely: there is no moment when the seed is knowable on chain and the
//! address is still free. What remains is a same-slot race against a transaction
//! that has not landed yet, and it is a bad one to be on the attacking side of.
//! Winning it only *reverts* the crank's transaction - the round is still open,
//! and the next attempt derives a different address.
//!
//! ## What the caller picks, and why that is safe
//!
//! Solana needs every account address before execution, and the ORAO address is a
//! function of the seed, so the caller cannot let the program surprise it with a
//! seed. It therefore names the slot whose hash goes in: `draw_round` takes
//! `seed_slot`, looks the hash up in `SlotHashes`, and refuses anything older
//! than [`vrf::SLOT_HASH_LOOKBACK`]. The rest of the seed is the round's own
//! committed entropy, its ids, and the signer.
//!
//! That same requirement - the address has to be known before the transaction
//! runs - is why none of those inputs may move while a draw is in flight. Every
//! one of them is either the caller's own choice or fixed when the round opened;
//! in particular `round.entropy` is written by the round's first member and then
//! left alone. It used to accumulate over every arrival, which quietly made the
//! address a function of mempool ordering: a push landing between a crank reading
//! the round and its draw executing moved the seed, so the draw reverted on the
//! address check. That needed no attacker, only traffic, and it got worse the
//! busier the star was - with the batch expiring into refunds if no attempt ever
//! won the gap. Now a late arrival simply joins the round and shares its draw.
//!
//! Choosing a seed is not choosing a draw. The draw does not exist until ORAO
//! answers a request this instruction had to create from nothing, and an address
//! that already holds an answer cannot be created - it fails the emptiness check
//! below - so a seed whose outcome is already known can never be adopted. Which
//! of the last 150 slot hashes was used is therefore free information, and so is
//! the signer.
//!
//! The signer being an input is deliberate: a griefer who wants to squat the
//! address has to know which wallet will sign for it, so a crank that uses a
//! fresh keypair per draw is not guessable at all. That is worth more than
//! narrowing the caller's seed choice would be, because narrowing it only matters
//! against an ORAO that is actively conspiring - and an ORAO that conspires can
//! simply withhold answers until it likes a round, or lie about the randomness
//! outright. Trusting the VRF is the trust assumption; this is not where it is
//! won or lost.
//!
//! ## Why a round can refuse to draw
//!
//! A draw costs the same whether one player or twenty are waiting on it, and the
//! house pays for it out of its own rake. So there is a stake below which a
//! round is not worth drawing: at 3.14% of stake against ORAO's price, one
//! minimum push does not pay for a request, and buying one anyway would mean the
//! float bleeds faster the more people play - the wrong sign on the whole thing.
//!
//! `draw_round` therefore checks the batch against the live price and, if it
//! falls short, declines. The round stays open and keeps taking members. Nothing
//! is stuck: the only thing that happened is that the draw was not bought yet,
//! and [`expire_round`] still voids the round into full refunds if the stake
//! never shows up. The house's edge is structural as a result - every draw it
//! buys was paid for by the stake behind that draw - and the game needs no
//! minimum bet to stay solvent, only patience on quiet stars.

use anchor_lang::prelude::*;

use crate::constants::{CONFIG_SEED, ROUND_SEED, STAR_SEED, VAULT_SEED};
use crate::errors::SoldustError;
use crate::events::{RoundClosed, RoundExpired, RoundRequested, RoundSwept};
use crate::math::{add, sub};
use crate::state::{Config, Round, RoundStatus, Star};
use crate::{game_config, vault, vrf};

// ------------------------------------------------------- sealing and drawing

/// Seal a round and buy the one ORAO request that serves all of its members.
///
/// The signer fronts ORAO and is reimbursed out of `protocol_accrued` - the
/// house's own rake, not anyone's stake. That is the whole economic point of
/// batching: a draw costs the same whether one player or twenty are waiting on
/// it, so charging it to the pot per-push made small pushes absurd while
/// charging it to the rake per-round makes them fine.
///
/// Reimbursement is capped at what the rake actually holds, so the instruction
/// cannot fail for lack of funds and stall the game. A crank that runs while the
/// till is empty - at genesis, before any push has settled - eats the
/// difference. The operator's own crank is expected to carry that; a third party
/// can read `Config.protocol_accrued` first if it cares.
#[derive(Accounts)]
pub struct DrawRound<'info> {
    /// Anyone. Fronts ORAO's price and is reimbursed from the rake.
    #[account(mut)]
    pub cranker: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    #[account(
        mut,
        seeds = [STAR_SEED, &round.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    #[account(
        mut,
        seeds = [ROUND_SEED, &round.star_id.to_le_bytes(), &round.round_id.to_le_bytes()],
        bump = round.bump,
    )]
    pub round: Account<'info, Round>,

    /// CHECK: pinned to the SlotHashes sysvar and parsed read-only in
    /// `vrf::slot_hash_at`. Sampled here for seed material only.
    pub slot_hashes: UncheckedAccount<'info>,

    /// CHECK: pinned to the ORAO program id.
    #[account(address = vrf::ORAO_VRF_PROGRAM_ID @ SoldustError::InvalidVrfProgram)]
    pub vrf_program: UncheckedAccount<'info>,

    /// CHECK: address verified in `vrf::draw_cost_estimate` and again in the CPI
    /// helper. Read to price the draw, then written by ORAO.
    #[account(mut)]
    pub vrf_network_state: UncheckedAccount<'info>,

    /// CHECK: pinned in the handler to the treasury ORAO's own network state
    /// names, which is also what ORAO constrains it against.
    #[account(mut)]
    pub vrf_treasury: UncheckedAccount<'info>,

    /// CHECK: created by ORAO during the CPI. Cannot be pinned by an `address`
    /// constraint because the seed that derives it is computed in the handler;
    /// `vrf::request_randomness` verifies it against that seed instead.
    #[account(mut)]
    pub vrf_request: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn draw_round(ctx: Context<DrawRound>, seed_slot: u64) -> Result<()> {
    let clock = Clock::get()?;

    require!(ctx.accounts.round.is_open(), SoldustError::RoundNotOpen);
    require!(
        ctx.accounts.round.member_count > 0,
        SoldustError::RoundEmpty
    );
    // Only the star's *current* round may be sealed. Anything older has been
    // sealed already, so this can only ever be the one the counter points at.
    require!(
        ctx.accounts.round.round_id == ctx.accounts.star.current_round,
        SoldustError::NotCurrentRound
    );
    require!(
        ctx.accounts.round.closeable_at(clock.slot),
        SoldustError::RoundNotCloseable
    );

    // Never buy randomness for a star that is already finished. Those members do
    // not roll - `resolve_push` refunds them on `star.is_finished()` before it
    // looks at the round at all - so a draw here spends the float on a number no
    // path will ever read, and delays the refund while it does. The mirror of the
    // `is_full_and_live` exemption below, which spells out the same distinction
    // from the other side.
    require!(ctx.accounts.star.is_alive(), SoldustError::StarNotAlive);

    // The economic gate. This is what commits the house to buying a draw, so
    // this is where the draw has to be shown to be worth buying: the round's own
    // rake must cover ORAO at ORAO's current price.
    //
    // Failing here is not an error condition, it is the mechanism. The round
    // stays open, `star.current_round` still points at it, and later pushes keep
    // joining and adding stake until the bar is cleared. The window is therefore
    // a *minimum* wait rather than a maximum, and a quiet star batches harder
    // instead of costing the house money - which is what lets the game run
    // unattended without the rake ever going backwards, at any push size and
    // whatever ORAO decides to charge.
    let draw_cost = vrf::draw_cost_estimate(&ctx.accounts.vrf_network_state)?;
    let rake = ctx
        .accounts
        .round
        .rake(game_config::economics().protocol_bps);

    // Unless the round cannot grow and its members still need a roll. A star
    // full to the hole cap rejects every further push, so an under-funded round
    // there is not waiting for company that could still arrive - it is waiting
    // forever, and the star would never take its final roll. That strands the
    // last fill one step below the boundary, which is exactly the class of freeze
    // this design exists to rule out, so the house eats one short draw instead.
    //
    // Once per *fill*, not strictly once per star: a refund from an expired round
    // can drop committed mass back under the cap, and filling it again buys
    // another short draw. Someone determined could cycle that, but each turn
    // locks them ~20 SOL of escrow for the whole expiry window to cost the house
    // a fraction of a cent, so the bound that matters is economic rather than
    // structural.
    let forced = ctx.accounts.star.is_full_and_live();
    require!(forced || rake >= draw_cost, SoldustError::RoundBelowDrawCost);

    // The seed is born here and spent below, in this same transaction. See the
    // module docs: the gap between those two used to be the attack, and why the
    // caller gets to name the slot rather than the program sampling it.
    let slot_hash = vrf::slot_hash_at(&ctx.accounts.slot_hashes, seed_slot)?;
    let star_id = ctx.accounts.round.star_id;
    let round_id = ctx.accounts.round.round_id;
    let seed = vrf::round_seed(
        star_id,
        round_id,
        &ctx.accounts.round.entropy,
        seed_slot,
        &slot_hash,
        &ctx.accounts.cranker.key(),
    );
    let randomness = vrf::randomness_address(&seed);

    // A round is requested exactly once, so anything already living at that
    // address is not ours. Refusing - rather than adopting whatever is sitting
    // there - is what stops a griefer handing a round a draw they have already
    // read. Reachable only by winning a same-slot race against this very
    // transaction, and the cost of losing it is that this transaction reverts:
    // the round stays open and the next attempt derives a different address.
    require_keys_eq!(
        ctx.accounts.vrf_request.key(),
        randomness,
        SoldustError::RandomnessAccountMismatch
    );
    require!(
        ctx.accounts.vrf_request.lamports() == 0
            && *ctx.accounts.vrf_request.owner == anchor_lang::solana_program::system_program::ID,
        SoldustError::VrfAlreadyRequested
    );

    // The fee has to leave the cranker for good, not go round in a circle. See
    // `vrf::require_network_treasury`: the reimbursement below is measured as the
    // cranker's balance delta, so a fee paid into an account the cranker owns
    // would still be charged to the rake.
    vrf::require_network_treasury(
        &ctx.accounts.vrf_network_state,
        &ctx.accounts.vrf_treasury.key(),
    )?;

    // Buy first, then price it. Measuring the signer's balance across the CPI
    // is the only way to know what ORAO charged that cannot go stale: both the
    // request fee and the rent rate are live cluster state.
    let before = ctx.accounts.cranker.lamports();
    vrf::request_randomness(
        &ctx.accounts.vrf_program.to_account_info(),
        &ctx.accounts.cranker.to_account_info(),
        &ctx.accounts.vrf_network_state.to_account_info(),
        &ctx.accounts.vrf_treasury.to_account_info(),
        &ctx.accounts.vrf_request.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        seed,
    )?;
    let outlay = before.saturating_sub(ctx.accounts.cranker.lamports());

    // Most of that outlay is rent ORAO returns to the signer directly when the
    // oracles answer, so only the remainder is reimbursed.
    let cost = vrf::unrecovered_cost(outlay, &ctx.accounts.vrf_request.to_account_info())?;
    let paid = cost.min(ctx.accounts.config.protocol_accrued);

    // Latch before paying: a failed transfer reverts the whole transaction
    // rather than losing the claim.
    {
        let round = &mut ctx.accounts.round;
        round.seed = seed;
        round.seed_slot = seed_slot;
        round.randomness = randomness;
        round.status = RoundStatus::Requested;
        round.closed_slot = clock.slot;
        round.requested_slot = clock.slot;
    }
    // Sealed and superseded in the same instruction: from here on
    // `request_push` derives a different round PDA, so this membership list can
    // never grow again.
    ctx.accounts.star.current_round = add(round_id, 1)?;
    {
        let config = &mut ctx.accounts.config;
        config.protocol_accrued = sub(config.protocol_accrued, paid)?;
    }

    vault::pay(
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.cranker.to_account_info(),
        ctx.accounts.config.vault_bump,
        paid,
    )?;

    let members = ctx.accounts.round.member_count;
    // Two events for one instruction, because they answer different questions and
    // both have readers: the seal is the auditable record of *how the seed came
    // to be*, and the request is the record of what the draw cost the house.
    emit!(RoundClosed {
        star_id,
        round_id,
        round: ctx.accounts.round.key(),
        seed,
        seed_slot,
        randomness,
        member_count: members,
        first_push_id: ctx.accounts.round.first_push_id,
        stake: ctx.accounts.round.stake,
        rake,
        draw_cost,
        forced,
        closed_slot: clock.slot,
        closed_ts: clock.unix_timestamp,
    });
    emit!(RoundRequested {
        star_id,
        round_id,
        round: ctx.accounts.round.key(),
        payer: ctx.accounts.cranker.key(),
        randomness,
        cost,
        reimbursed: paid,
        member_count: members,
        // What the batching actually saved, in the unit that matters.
        cost_per_member: cost / members.max(1),
        requested_slot: clock.slot,
    });
    Ok(())
}

// -------------------------------------------------------------------- voiding

/// Void a round that has stalled, so its members can take their stake back.
///
/// This is the reason a star cannot be frozen forever. Two things can stall a
/// round - nobody buys its draw, or ORAO never answers - and both end here after
/// [`ROUND_EXPIRY_SLOTS`](crate::constants::ROUND_EXPIRY_SLOTS).
/// Every member then refunds in full through `resolve_push`, in `push_id` order
/// like any other resolution, without needing randomness, and the star carries
/// on with a fresh round.
///
/// It is always taken blind. A round whose draw has already landed cannot be
/// voided, so nobody can look at an unfavourable roll and cancel out of it; and
/// while the draw is still pending, by definition nobody knows what it says.
#[derive(Accounts)]
pub struct ExpireRound<'info> {
    pub cranker: Signer<'info>,

    #[account(
        mut,
        seeds = [STAR_SEED, &round.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    #[account(
        mut,
        seeds = [ROUND_SEED, &round.star_id.to_le_bytes(), &round.round_id.to_le_bytes()],
        bump = round.bump,
    )]
    pub round: Account<'info, Round>,

    /// CHECK: ORAO randomness for this round. Read-only, and only to confirm
    /// the draw has *not* landed. Ignored while the round never got as far as
    /// being requested, in which case the address is still default.
    pub vrf_request: UncheckedAccount<'info>,
}

pub fn expire_round(ctx: Context<ExpireRound>) -> Result<()> {
    let clock = Clock::get()?;

    require!(
        ctx.accounts.round.status != RoundStatus::Expired,
        SoldustError::RoundExpired
    );
    require!(
        ctx.accounts.round.member_count > 0,
        SoldustError::RoundEmpty
    );
    require!(
        ctx.accounts.round.expirable_at(clock.slot),
        SoldustError::RoundNotExpired
    );

    // A landed draw is a usable draw, however late it was: settle it, do not
    // void it. Only meaningful once the round was actually requested - before
    // that `round.randomness` is either default or points at an account ORAO
    // never created.
    //
    // A draw that cannot be *read* is not a landed draw. `read_fulfilled` errors
    // rather than returning `None` for a wrong owner, a moved discriminator, an
    // unknown enum tag, a short account or a seed mismatch, and propagating any
    // of those here would close the escape hatch precisely when it is needed:
    // the whole point of this instruction is to get players out when the oracle
    // is the thing that broke, and "answered in a shape we cannot parse" is one
    // of the ways it breaks. So an unreadable account voids the round, which is
    // safe in both directions - the address is pinned to `round.randomness`, so
    // nobody can substitute a broken account to duck a roll, and the settle path
    // refuses the same account anyway.
    if ctx.accounts.round.status == RoundStatus::Requested {
        require_keys_eq!(
            ctx.accounts.vrf_request.key(),
            ctx.accounts.round.randomness,
            SoldustError::RandomnessAccountMismatch
        );
        let landed = vrf::read_fulfilled(
            &ctx.accounts.vrf_request.to_account_info(),
            &ctx.accounts.round.seed,
        )
        .unwrap_or(None);
        require!(landed.is_none(), SoldustError::RoundNotExpired);
    }

    // A round that never got sealed still holds the star's round counter, so
    // release it here or new pushes would keep piling into a dead batch.
    let was_open = ctx.accounts.round.is_open();
    if was_open {
        require!(
            ctx.accounts.round.round_id == ctx.accounts.star.current_round,
            SoldustError::NotCurrentRound
        );
        ctx.accounts.star.current_round = add(ctx.accounts.round.round_id, 1)?;
    }

    let round = &mut ctx.accounts.round;
    round.status = RoundStatus::Expired;

    emit!(RoundExpired {
        star_id: round.star_id,
        round_id: round.round_id,
        round: round.key(),
        member_count: round.member_count,
        first_push_id: round.first_push_id,
        never_sealed: was_open,
        expired_slot: clock.slot,
        expired_ts: clock.unix_timestamp,
    });
    Ok(())
}

// ------------------------------------------------------------------- sweeping

/// Give a drained round's rent back to the member who paid for it.
///
/// A `Round` account is created by whoever happens to push first into a fresh
/// batch, and its rent comes out of that player's wallet. Everyone else in the
/// round rides along for free. Nothing about being first is worth paying for, so
/// leaving that asymmetry in place made a minimum push on a quiet star cost more
/// than the push - the player got their whole stake back and was still down the
/// rent for an account they had no idea they had opened.
///
/// So the account is disposable, and this returns it. Permissionless, because the
/// recipient is fixed at `opened_by` and there is nothing to choose: a crank can
/// sweep it alongside `close_push` and the rent finds the right wallet either way.
///
/// Two conditions, and both are needed:
///
/// * **Every member has resolved.** Otherwise a member would lose the account
///   their own resolution reads. [`Round::is_drained`] is exact here; see its
///   docs for why the settle cursor is enough.
/// * **The round can never take another member.** Closing an account does not
///   stop `request_push` recreating it - it uses `init_if_needed` - so a round
///   still open at the front of the queue has to stay put. Either the star has
///   moved on to a later round, or the star is finished and takes no pushes at
///   all.
#[derive(Accounts)]
pub struct CloseRoundAccount<'info> {
    /// Anyone. Gets nothing; the rent goes to `opened_by`.
    pub cranker: Signer<'info>,

    #[account(
        seeds = [STAR_SEED, &round.star_id.to_le_bytes()],
        bump = star.bump,
    )]
    pub star: Account<'info, Star>,

    #[account(
        mut,
        close = rent_recipient,
        seeds = [ROUND_SEED, &round.star_id.to_le_bytes(), &round.round_id.to_le_bytes()],
        bump = round.bump,
    )]
    pub round: Account<'info, Round>,

    /// CHECK: pinned to the round's recorded opener. Receives lamports only.
    #[account(mut, address = round.opened_by @ SoldustError::PushPlayerMismatch)]
    pub rent_recipient: UncheckedAccount<'info>,
}

pub fn close_round_account(ctx: Context<CloseRoundAccount>) -> Result<()> {
    let star = &ctx.accounts.star;
    let round = &ctx.accounts.round;

    require!(
        round.round_id != star.current_round || !star.is_alive(),
        SoldustError::RoundNotDrained
    );
    require!(
        round.is_drained(star.settle_cursor),
        SoldustError::RoundNotDrained
    );

    emit!(RoundSwept {
        star_id: round.star_id,
        round_id: round.round_id,
        round: round.key(),
        rent_recipient: round.opened_by,
        lamports: round.to_account_info().lamports(),
        member_count: round.member_count,
    });
    Ok(())
}
