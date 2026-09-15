//! The single SOL vault at `["vault"]`.
//!
//! It is a plain system-owned account with no data, so lamports can only
//! leave it through a `system_program::transfer` signed by the vault PDA -
//! i.e. only through this program. Every lamport in it is covered by exactly
//! one of four buckets tracked on `Config`:
//!
//! * `pending_liability`  - escrowed pushes awaiting resolution
//! * `prize_liability`    - prizes owed to star killers
//! * `next_star_reserve`  - recycled stake that seeds the next star
//! * `protocol_accrued`   - protocol revenue, and the float randomness is
//!                          bought from; the only withdrawable bucket
//!
//! plus the account's own rent-exempt minimum. The first three are
//! `Config::reserved()` and `withdraw_protocol_fees` cannot reach into them.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer};

use crate::constants::VAULT_SEED;

pub fn pay<'info>(
    system_program_ai: &AccountInfo<'info>,
    vault: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    vault_bump: u8,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let seeds: &[&[u8]] = &[VAULT_SEED, core::slice::from_ref(&vault_bump)];
    system_program::transfer(
        CpiContext::new_with_signer(
            system_program_ai.key(),
            Transfer {
                from: vault.clone(),
                to: to.clone(),
            },
            &[seeds],
        ),
        amount,
    )
}
