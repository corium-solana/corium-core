//! The round lifecycle: draw, void, or sweep.
//!
//! All of it is permissionless. A round moves `Open -> Requested`, or sideways to
//! `Expired` if it stalls, and never backwards.
//!
//! ```text
//!   request_push ...              members join, stake accumulates, seed fixed
//!        |
//!        |  draw_round         window elapsed or target size reached, AND the
//!        |                     batch's rake covers the request fee.
//!        v                     seed = H(entropy, slot_hash, cranker) and the
//!    Requested                 one VRF request for the batch, same transaction
//!        |
//!        |  an oracle answers, and the VRF program calls back into
//!        |  consume_randomness with the result
//!        v
//!      Drawn                   the draw is now a field on the round
//!        |
//!        |  resolve_push x N   each member rolls H(draw, push_id)
//!        v
//!        |  close_round_account once every member has resolved
//!        v
//!     (rent back to the member who opened it)
//! ```
//!
//! ## Why sealing and requesting are one transaction
//!
//! They used to be two, on the theory that publishing the seed before anyone
//! paid for it removed all discretion from the purchase. Under the pull-based
//! ORAO integration that split was actively dangerous: the randomness account's
//! address was a function of the seed and first-come-first-served, so publishing
//! a sealed seed let anyone occupy that address and leave the round permanently
//! undrawable.
//!
//! MagicBlock has no per-request address, so that hazard is simply gone - there
//! is nothing to squat. The atomicity is kept anyway, because it is free and it
//! preserves the cleaner property: the seed is decided in the very transaction
//! that spends it, so it is never public while still unspent. Nothing to grind
//! at either end.
//!
//! ## What the caller picks, and why that is safe
//!
//! `draw_round` takes `seed_slot`, looks its hash up in `SlotHashes`, and
//! refuses anything older than [`vrf::SLOT_HASH_LOOKBACK`]. The rest of the seed
//! is the round's own committed entropy, its ids, and the signer.
//!
//! The caller naming the slot is now a convenience rather than a requirement -
//! it is what makes the derivation replayable off-chain from public data, and
//! what keeps the published test vectors meaningful. Either way the choice is
//! bounded to real, already-final chain state, and choosing among known hashes
//! buys nothing: none of them tells you what the oracle will answer.
//!
//! `round.entropy` is still written by the round's first member and then left
//! alone. It used to accumulate over every arrival, which made the seed a
//! function of mempool ordering and reverted draws under mere traffic. Freezing
//! it costs nothing and a late arrival simply joins the round and shares its
//! draw.
//!
//! Choosing a seed is not choosing a draw. The draw does not exist until an
//! oracle answers a request this instruction had to file from nothing, and the
//! only way that answer reaches a round is a callback the VRF program signed
//! for. Which of the last 150 slot hashes was used is therefore free
//! information, and so is the signer.
//!
//! An oracle that conspires can withhold answers until it likes a round, or lie
//! about the randomness outright. Trusting the VRF is the trust assumption; none
//! of the above is where it is won or lost.
//!
//! ## Why a round can refuse to draw
//!
//! A draw costs the same whether one player or twenty are waiting on it, and the
//! house pays for it out of its own rake. So there is a stake below which a
//! round is not worth drawing: at 3.14% of stake against a 500_000-lamport
//! request, one minimum push still does not pay for a request, and buying one
//! anyway would mean the float bleeds faster the more people play - the wrong
//! sign on the whole thing.
//!
//! `draw_round` therefore checks the batch against the price and, if it falls
//! short, declines. The round stays open and keeps taking members. Nothing is
//! stuck: the only thing that happened is that the draw was not bought yet, and
//! [`expire_round`] still voids the round into full refunds if the stake never
//! shows up. The house's edge is structural as a result - every draw it buys was
//! paid for by the stake behind that draw - and the game needs no minimum bet to
//! stay solvent, only patience on quiet stars.
//!
//! Dropping ORAO cut the bar from seven minimum pushes to two, because ORAO
//! entombed ~0.00168 SOL of unrecoverable rent in every draw on top of its fee.
//! The gate stays regardless: it is one comparison, and it is what keeps
//! solvency independent of a price somebody else sets and can change.

use anchor_lang::prelude::*;

use crate::constants::{CONFIG_SEED, ROUND_SEED, STAR_SEED, VAULT_SEED};
use crate::errors::SoldustError;
use crate::events::{RoundClosed, RoundDrawn, RoundExpired, RoundRequested, RoundSwept};
use crate::math::{add, sub};
use crate::state::{Config, Round, RoundStatus, Star};
use crate::{game_config, vault, vrf};

// ------------------------------------------------------- sealing and drawing

/// Seal a round and buy the one VRF request that serves all of its members.
///
/// The signer fronts the fee and is reimbursed out of `protocol_accrued` - the
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
    /// Anyone. Fronts the request fee and is reimbursed from the rake.
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

    /// CHECK: pinned to the MagicBlock VRF program id.
    #[account(address = vrf::MAGICBLOCK_VRF_PROGRAM_ID @ SoldustError::InvalidVrfProgram)]
    pub vrf_program: UncheckedAccount<'info>,

    /// CHECK: this program's own request identity, `PDA(["identity"], soldust)`.
    /// Never created and holds nothing - it exists only as a signature the VRF
    /// program checks to learn which program the callback belongs to, and
    /// `draw_round` produces that signature with `invoke_signed`.
    #[account(seeds = [vrf::IDENTITY_SEED], bump)]
    pub vrf_identity: UncheckedAccount<'info>,

    /// CHECK: pinned to `vrf::VRF_QUEUE`, which is also where the request fee
    /// lands. That pin protects the house's float rather than MagicBlock's:
    /// anyone may stand up a queue of their own, so without it a cranker could
    /// file against a queue they control, be reimbursed from the rake, and
    /// recover the fee by closing it. See `vrf::VRF_QUEUE`.
    #[account(mut, address = vrf::VRF_QUEUE @ SoldustError::InvalidVrfQueue)]
    pub vrf_queue: UncheckedAccount<'info>,

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
    // rake must cover the request fee.
    //
    // Failing here is not an error condition, it is the mechanism. The round
    // stays open, `star.current_round` still points at it, and later pushes keep
    // joining and adding stake until the bar is cleared. The window is therefore
    // a *minimum* wait rather than a maximum, and a quiet star batches harder
    // instead of costing the house money - which is what lets the game run
    // unattended without the rake ever going backwards, at any push size.
    let draw_cost = vrf::draw_cost_estimate();
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

    // File the request, naming this round as the one account the callback may
    // write. That binding is made here and cannot be changed afterwards, which
    // is half of why a draw can only ever reach the round that asked for it;
    // the other half is the identity signature `consume_randomness` checks.
    //
    // Buy first, then price it. Measuring the signer's balance across the CPI
    // is the only way to know what was actually charged that cannot go stale -
    // the fee is a constant in somebody else's upgradeable program.
    let before = ctx.accounts.cranker.lamports();
    vrf::request_randomness(
        &ctx.accounts.vrf_program.to_account_info(),
        &ctx.accounts.cranker.to_account_info(),
        &ctx.accounts.vrf_identity.to_account_info(),
        ctx.bumps.vrf_identity,
        &ctx.accounts.vrf_queue.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.slot_hashes.to_account_info(),
        &ctx.accounts.round.key(),
        seed,
    )?;
    let outlay = before.saturating_sub(ctx.accounts.cranker.lamports());

    // Nothing is held back and nothing comes back - there is no per-request
    // account any more - so the whole outlay is the cost.
    let cost = vrf::unrecovered_cost(outlay);
    let paid = cost.min(ctx.accounts.config.protocol_accrued);

    // Latch before paying: a failed transfer reverts the whole transaction
    // rather than losing the claim.
    {
        let round = &mut ctx.accounts.round;
        round.seed = seed;
        round.seed_slot = seed_slot;
        // `randomness` stays zero: the draw does not exist yet, and only
        // `consume_randomness` may write it.
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
        cost,
        reimbursed: paid,
        member_count: members,
        // What the batching actually saved, in the unit that matters.
        cost_per_member: cost / members.max(1),
        requested_slot: clock.slot,
    });
    Ok(())
}

// ------------------------------------------------------------------- callback

/// Receive a round's draw from the VRF program.
///
/// This is the one instruction in the program an outside program invokes, and
/// the only writer of [`RoundStatus::Drawn`]. It moves no money, touches no
/// star and no push: it writes 32 bytes and a status, and everything else is
/// decided later by `resolve_push` reading them.
///
/// ## Why this cannot be forged
///
/// Three independent facts have to hold before a draw lands on a round, and
/// each one is checked here or fixed at request time:
///
/// * **The caller is the VRF program.** `vrf_identity` is pinned to
///   [`vrf::callback_identity`], a PDA derived under the VRF program's own id
///   and scoped to *this* program. Only that program can produce a signature
///   for it, and the VRF program only does so after verifying an oracle's RFC
///   9381 proof against a request in its queue. This constraint is the whole of
///   the integrity argument; it is the direct successor of the ORAO
///   integration's derived-address pin.
/// * **The round is the one that asked.** `draw_round` names this round in the
///   request's callback account list, so the VRF program can only hand the
///   result back to that round. The seeds constraint below additionally proves
///   the account is the PDA its own contents claim to be.
/// * **The draw is written once.** Only a `Requested` round is accepted, and
///   this instruction leaves it `Drawn`, so a replayed callback finds the wrong
///   status and fails. The same check rejects a late callback for a round that
///   already expired - whose members may already have been refunded - which is
///   the case that would otherwise pay out twice.
///
/// Nothing here trusts the randomness to be *good*; that is the VRF's job and
/// the trust assumption. What is enforced is that it is the answer to the
/// question this round actually asked.
#[derive(Accounts)]
pub struct ConsumeRandomness<'info> {
    /// The VRF program's scoped identity for this program, signing the callback.
    ///
    /// Account 0 because that is where the VRF program puts it: it builds the
    /// callback as `[identity] ++ the accounts named at request time`.
    #[account(
        address = vrf::callback_identity() @ SoldustError::InvalidVrfCallbackIdentity,
    )]
    pub vrf_identity: Signer<'info>,

    #[account(
        mut,
        seeds = [ROUND_SEED, &round.star_id.to_le_bytes(), &round.round_id.to_le_bytes()],
        bump = round.bump,
    )]
    pub round: Account<'info, Round>,
}

pub fn consume_randomness(ctx: Context<ConsumeRandomness>, randomness: [u8; 32]) -> Result<()> {
    let clock = Clock::get()?;

    // Write-once, and the reason an expired round cannot be revived: `Expired`
    // is not `Requested`, so a draw that arrives after the escape hatch fired
    // is refused rather than settled against members who already refunded.
    require!(
        ctx.accounts.round.status == RoundStatus::Requested,
        SoldustError::RoundNotRequested
    );

    // All-zero is the byte pattern an undrawn round already carries, so storing
    // it would make `Drawn` and "no draw yet" indistinguishable to every reader
    // off chain. The oracle hands us `sha256(vrf_output)`, so this is
    // unreachable in practice - it is here to keep that convention total rather
    // than because a zero draw is expected.
    require!(randomness != [0u8; 32], SoldustError::ZeroRandomness);

    let round = &mut ctx.accounts.round;
    round.randomness = randomness;
    round.status = RoundStatus::Drawn;

    emit!(RoundDrawn {
        star_id: round.star_id,
        round_id: round.round_id,
        round: round.key(),
        seed: round.seed,
        randomness,
        member_count: round.member_count,
        first_push_id: round.first_push_id,
        requested_slot: round.requested_slot,
        drawn_slot: clock.slot,
        drawn_ts: clock.unix_timestamp,
    });
    Ok(())
}

// -------------------------------------------------------------------- voiding

/// Void a round that has stalled, so its members can take their stake back.
///
/// This is the reason a star cannot be frozen forever. Two things can stall a
/// round - nobody buys its draw, or no oracle ever answers - and both end here
/// after [`ROUND_EXPIRY_SLOTS`](crate::constants::ROUND_EXPIRY_SLOTS).
/// Every member then refunds in full through `resolve_push`, in `push_id` order
/// like any other resolution, without needing randomness, and the star carries
/// on with a fresh round.
///
/// It is always taken blind. A round whose draw has already landed cannot be
/// voided, so nobody can look at an unfavourable roll and cancel out of it; and
/// while the draw is still pending, by definition nobody knows what it says.
///
/// That guarantee used to require parsing ORAO's account and deciding what an
/// unreadable answer meant - a defensive branch that had to void the round,
/// because refusing to would have closed the escape hatch exactly when the
/// oracle was the broken thing. Under the push model it is a status comparison:
/// [`Round::stalled_since`] puts a `Drawn` round beyond any expiry slot, so a
/// landed draw is structurally unvoidable and there is no foreign layout left
/// to get wrong.
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
    // void it. `expirable_at` above already enforces that - a `Drawn` round
    // reports an unreachable stall slot - so there is nothing further to check
    // here and no oracle account to read.

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
