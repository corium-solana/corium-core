//! The protocol balance: money in, money out.
//!
//! `protocol_accrued` does double duty. It is the rake, withdrawable only to the
//! treasury frozen at initialize, and it is also the float `draw_round`
//! reimburses cranks for randomness out of. That is deliberate: the house pays
//! for the oracle out of its own take, so a player's stake is never exposed to
//! what the oracle charges, and the two flows net against each other
//! automatically.
//!
//! It is also why the withdrawal is permissionless only down to
//! [`DRAW_FLOAT_FLOOR`](crate::constants::DRAW_FLOAT_FLOOR). Collecting revenue
//! should not require the house to be online; emptying the float it buys
//! randomness with should require the treasury's own signature.
//!
//! Neither instruction here can retune the curve, pause the game, move
//! authority, or reach a single lamport of `Config::reserved()`.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

use crate::constants::{CONFIG_SEED, DRAW_FLOAT_FLOOR, VAULT_SEED};
use crate::errors::SoldustError;
use crate::events::{ProtocolFeesWithdrawn, ProtocolFunded};
use crate::math::{add, sub};
use crate::state::Config;
use crate::vault;

/// Two callers, one instruction. Anyone may crank a withdrawal down to
/// [`DRAW_FLOAT_FLOOR`]; the treasury, signing for itself, may take everything.
///
/// The distinction exists because this balance does double duty. It is revenue,
/// which is why collecting it is permissionless - the house should never have to
/// be online to be paid - and it is also the float `draw_round` reimburses cranks
/// from, which is why a stranger must not be able to empty it. See the constant.
#[derive(Accounts)]
pub struct WithdrawProtocolFees<'info> {
    /// Anyone. Pays the transaction fee, and gets nothing for it - the money
    /// always goes to `treasury`. Signing as the treasury itself is what lifts
    /// the float floor, so there is nothing here worth impersonating: the payout
    /// address does not move either way.
    pub crank: Signer<'info>,

    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = treasury,
    )]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    /// CHECK: pinned to `config.treasury` by `has_one`. Set once at initialize.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn withdraw_protocol_fees(ctx: Context<WithdrawProtocolFees>, amount: u64) -> Result<()> {
    let reserved = ctx.accounts.config.reserved()?;
    let rent_minimum = Rent::get()?.minimum_balance(0);
    let balance = ctx.accounts.vault.lamports();

    require!(
        amount <= ctx.accounts.config.protocol_accrued,
        SoldustError::ExceedsAccruedFees
    );

    // The float floor. Only the treasury may take a balance below it, and only by
    // signing: being the *destination* is not enough, since the address is frozen
    // and anyone may name it.
    if ctx.accounts.crank.key() != ctx.accounts.config.treasury {
        let collectable = ctx
            .accounts
            .config
            .protocol_accrued
            .saturating_sub(DRAW_FLOAT_FLOOR);
        require!(amount <= collectable, SoldustError::WouldDrainDrawFloat);
    }

    let withdrawable = balance
        .saturating_sub(reserved)
        .saturating_sub(rent_minimum);
    require!(
        amount <= withdrawable,
        SoldustError::InsufficientUnreservedFunds
    );

    let vault_bump = ctx.accounts.config.vault_bump;
    vault::pay(
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.treasury.to_account_info(),
        vault_bump,
        amount,
    )?;

    let config = &mut ctx.accounts.config;
    config.protocol_accrued = sub(config.protocol_accrued, amount)?;

    emit!(ProtocolFeesWithdrawn {
        treasury: ctx.accounts.treasury.key(),
        amount,
        remaining_accrued: ctx.accounts.config.protocol_accrued,
    });
    Ok(())
}

// ------------------------------------------------------------------ money in

/// Top up the protocol balance, so randomness has something to be bought out of.
///
/// Needed exactly once, at bootstrap: the rake only accrues when a push settles,
/// a push can only settle once its round has a draw, and a draw has to be paid
/// for. Somebody has to break that circle by putting the first draw's worth of
/// lamports in. After that the game funds its own oracle.
///
/// Permissionless and one-way - this is a donation to the house, and the house
/// can withdraw it to the treasury like any other revenue. Nobody should call
/// it expecting anything back.
#[derive(Accounts)]
pub struct FundProtocol<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn fund_protocol(ctx: Context<FundProtocol>, amount: u64) -> Result<()> {
    require!(amount > 0, SoldustError::PushAmountOutOfRange);

    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            Transfer {
                from: ctx.accounts.payer.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        ),
        amount,
    )?;

    let config = &mut ctx.accounts.config;
    config.protocol_accrued = add(config.protocol_accrued, amount)?;

    emit!(ProtocolFunded {
        payer: ctx.accounts.payer.key(),
        amount,
        protocol_accrued: config.protocol_accrued,
    });
    Ok(())
}
