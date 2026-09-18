//! Create `Config` and the vault. Once, by whoever deployed the program.
//!
//! No authority is recorded, because there is no instruction an authority could
//! call. The only durable choice made here is the treasury, and it can never be
//! moved afterwards - which is exactly why *who* gets to make it matters.
//!
//! ## Why this one instruction has a gate
//!
//! Everything else in SOLDUST is permissionless on purpose. This is not, because
//! it is the one instruction whose effect cannot be redone: the treasury is
//! written once and pinned by `has_one` forever after. Leaving it open means the
//! first transaction to land after `solana program deploy` decides where all
//! protocol revenue goes for the life of the program, and that window is public.
//!
//! Rather than hardcode a pubkey that could be wrong, the gate asks the loader
//! who deployed this program: the signer must be the current BPF upgrade
//! authority. That cannot be stale and needs no configuration. The consequence
//! worth knowing is the ordering it implies - **initialize before dropping
//! upgrade authority to `None`**, because afterwards there is no authority to
//! match and the game could never be started.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

use crate::constants::{CONFIG_SEED, VAULT_SEED};
use crate::errors::SoldustError;
use crate::state::Config;

/// The BPF upgradeable loader, which owns this program's `ProgramData` account.
const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

// `UpgradeableLoaderState::ProgramData` on the wire (bincode, not Borsh):
//   [0..4)    enum tag: u32 = 3
//   [4..12)   slot: u64
//   [12]      Option tag: 0 = None, 1 = Some
//   [13..45)  upgrade_authority_address: Pubkey
const PROGRAM_DATA_TAG: u32 = 3;
const PD_OFF_OPTION: usize = 12;
const PD_OFF_AUTHORITY: usize = 13;
const PD_LEN: usize = 45;

/// This program's current upgrade authority, or `None` if it is already
/// immutable. Parsed by hand because it is a handful of fixed offsets and
/// cheaper than pulling in a deserializer for a loader account whose layout is
/// fixed by the runtime.
fn upgrade_authority(program_data: &AccountInfo) -> Result<Option<Pubkey>> {
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE,
        SoldustError::MalformedProgramData
    );
    let data = program_data.try_borrow_data()?;
    require!(data.len() >= PD_LEN, SoldustError::MalformedProgramData);

    let mut tag = [0u8; 4];
    tag.copy_from_slice(&data[..4]);
    require!(
        u32::from_le_bytes(tag) == PROGRAM_DATA_TAG,
        SoldustError::MalformedProgramData
    );

    match data[PD_OFF_OPTION] {
        0 => Ok(None),
        1 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(&data[PD_OFF_AUTHORITY..PD_OFF_AUTHORITY + 32]);
            Ok(Some(Pubkey::new_from_array(key)))
        }
        _ => err!(SoldustError::MalformedProgramData),
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    /// Pays rent for `Config` and tops the vault up to rent exemption. Must be
    /// the program's upgrade authority; see the module docs.
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        init,
        payer = payer,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, Config>,

    /// The one SOL vault. System-owned with zero data, so the program can
    /// move lamports out with a signed `system_program::transfer` and nothing
    /// else can touch it.
    #[account(mut, seeds = [VAULT_SEED], bump)]
    pub vault: SystemAccount<'info>,

    /// CHECK: pinned by seed to this program's own `ProgramData`, and read only
    /// to learn who deployed. Parsed in `upgrade_authority`.
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = BPF_LOADER_UPGRADEABLE,
    )]
    pub program_data: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn initialize(
    ctx: Context<Initialize>,
    genesis_seed: [u8; 32],
    treasury: Pubkey,
) -> Result<()> {
    // The one gate in the program. See the module docs for why it is here and
    // why it is the loader's answer rather than a compiled-in pubkey.
    let authority = upgrade_authority(&ctx.accounts.program_data.to_account_info())?;
    require!(
        authority == Some(ctx.accounts.payer.key()),
        SoldustError::NotProgramAuthority
    );

    // Keep the vault rent-exempt for its whole life. Without this the runtime
    // could reap it the moment the balance dips, taking escrow with it.
    let rent_minimum = Rent::get()?.minimum_balance(0);
    let current = ctx.accounts.vault.lamports();
    if current < rent_minimum {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                },
            ),
            rent_minimum - current,
        )?;
    }

    ctx.accounts.config.set_inner(Config {
        treasury,
        current_star_id: 0,
        stars_created: 0,
        genesis_seed,
        pending_liability: 0,
        prize_liability: 0,
        protocol_accrued: 0,
        next_star_reserve: 0,
        total_pushes_settled: 0,
        total_volume: 0,
        bump: ctx.bumps.config,
        vault_bump: ctx.bumps.vault,
    });

    msg!("SOLDUST initialized. treasury={}", treasury);
    Ok(())
}
