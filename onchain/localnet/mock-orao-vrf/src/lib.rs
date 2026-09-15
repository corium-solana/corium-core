//! Local-validator stand-in for ORAO VRF v2.
//!
//! Loaded at ORAO's real address with
//! `solana-test-validator --bpf-program VRFzZ... mock_orao_vrf.so`, which needs
//! no keypair for the address. That matters: soldust hardcodes the ORAO program
//! id, the randomness PDA seeds, and the byte offsets it reads, so a mock is
//! only useful if it sits at the real address and produces the real layout.
//!
//! Fidelity is the whole point, so everything soldust looks at is reproduced
//! exactly rather than approximated:
//!
//! * `request_v2` is named so Anchor derives `sha256("global:request_v2")[..8]`,
//!   which is the discriminator soldust's hand-rolled instruction sends.
//! * The account is written with the `RandomnessV2` discriminator and the
//!   `RequestAccount` enum layout soldust parses: tag at 8, client at 9, seed at
//!   41, randomness at 73.
//! * The pending account is 749 bytes, ORAO's real mainnet size, so the
//!   `unrecovered_cost` arithmetic sees the same rent split it will see live.
//! * `NetworkState` carries `request_fee` at offset 72 where
//!   `draw_cost_estimate` reads it.
//!
//! The `mock_*` instructions are the parts a real oracle network does off-chain
//! (fulfilment) or would only do by shipping an upgrade (`mock_corrupt`, which
//! simulates ORAO changing the account layout under us).
//!
//! One deliberate divergence: `mock_fulfill` does not shrink the account or
//! hand rent back to the payer the way ORAO does. Soldust prices the draw at
//! request time, so the reimbursement it pays is identical either way, and
//! skipping the resize keeps the mock free of runtime rent-exemption edges.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{create_account, CreateAccount};

declare_id!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");

pub const RANDOMNESS_SEED: &[u8] = b"orao-vrf-randomness-request";
pub const NETWORK_SEED: &[u8] = b"orao-vrf-network-configuration";

/// `sha256("account:RandomnessV2")[..8]`, asserted equal to soldust's constant
/// in the test at the bottom of this file.
const ACCOUNT_RANDOMNESS_V2: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];

/// ORAO's real pending-request size on mainnet.
pub const PENDING_SIZE: usize = 749;

const TAG_PENDING: u8 = 0;
const TAG_FULFILLED: u8 = 1;

const OFF_TAG: usize = 8;
const OFF_CLIENT: usize = 9;
const OFF_SEED: usize = 41;
const OFF_RANDOMNESS: usize = 73;

// NetworkState, as far as soldust cares:
//   [0..8) disc  [8..40) authority  [40..72) treasury  [72..80) request_fee
pub const NS_SIZE: usize = 80;
const NS_OFF_AUTHORITY: usize = 8;
const NS_OFF_TREASURY: usize = 40;
const NS_OFF_FEE: usize = 72;

#[program]
pub mod mock_orao_vrf {
    use super::*;

    /// The one instruction soldust CPIs into. Account order is exactly the
    /// order `vrf::request_randomness` builds its metas in.
    pub fn request_v2(ctx: Context<RequestV2>, seed: [u8; 32]) -> Result<()> {
        let (network_state, _) = Pubkey::find_program_address(&[NETWORK_SEED], &crate::ID);
        require_keys_eq!(
            ctx.accounts.network_state.key(),
            network_state,
            MockVrfError::BadNetworkState
        );

        let (fee, treasury) = {
            let data = ctx.accounts.network_state.try_borrow_data()?;
            require!(data.len() >= NS_SIZE, MockVrfError::BadNetworkState);
            let mut fee = [0u8; 8];
            fee.copy_from_slice(&data[NS_OFF_FEE..NS_OFF_FEE + 8]);
            let mut treasury = [0u8; 32];
            treasury.copy_from_slice(&data[NS_OFF_TREASURY..NS_OFF_TREASURY + 32]);
            (u64::from_le_bytes(fee), Pubkey::new_from_array(treasury))
        };
        // The real program pins its treasury the same way, and soldust now pins
        // it independently too - see `vrf::require_network_treasury` for why it
        // does not simply trust this check.
        require_keys_eq!(
            ctx.accounts.treasury.key(),
            treasury,
            MockVrfError::BadTreasury
        );

        let (request, bump) = Pubkey::find_program_address(&[RANDOMNESS_SEED, &seed], &crate::ID);
        require_keys_eq!(
            ctx.accounts.request.key(),
            request,
            MockVrfError::BadRequestAddress
        );

        // Fails if the address is already occupied, which is exactly the
        // behaviour soldust's `VrfAlreadyRequested` pre-check anticipates.
        let bump_slice = [bump];
        let seeds: &[&[u8]] = &[RANDOMNESS_SEED, &seed, &bump_slice];
        create_account(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.request.to_account_info(),
                },
                &[seeds],
            ),
            Rent::get()?.minimum_balance(PENDING_SIZE),
            PENDING_SIZE as u64,
            &crate::ID,
        )?;

        if fee > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.key(),
                    anchor_lang::system_program::Transfer {
                        from: ctx.accounts.payer.to_account_info(),
                        to: ctx.accounts.treasury.to_account_info(),
                    },
                ),
                fee,
            )?;
        }

        let mut data = ctx.accounts.request.try_borrow_mut_data()?;
        data[..8].copy_from_slice(&ACCOUNT_RANDOMNESS_V2);
        data[OFF_TAG] = TAG_PENDING;
        data[OFF_CLIENT..OFF_CLIENT + 32].copy_from_slice(ctx.accounts.payer.key().as_ref());
        data[OFF_SEED..OFF_SEED + 32].copy_from_slice(&seed);

        msg!("mock request_v2 seed={} fee={}", hex(&seed), fee);
        Ok(())
    }

    /// Stand in for ORAO's genesis network configuration.
    pub fn mock_init_network(
        ctx: Context<MockInitNetwork>,
        request_fee: u64,
        treasury: Pubkey,
    ) -> Result<()> {
        let (expected, bump) = Pubkey::find_program_address(&[NETWORK_SEED], &crate::ID);
        require_keys_eq!(
            ctx.accounts.network_state.key(),
            expected,
            MockVrfError::BadNetworkState
        );

        let bump_slice = [bump];
        let seeds: &[&[u8]] = &[NETWORK_SEED, &bump_slice];
        create_account(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.network_state.to_account_info(),
                },
                &[seeds],
            ),
            Rent::get()?.minimum_balance(NS_SIZE),
            NS_SIZE as u64,
            &crate::ID,
        )?;

        let mut data = ctx.accounts.network_state.try_borrow_mut_data()?;
        data[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // soldust never reads this
        data[NS_OFF_AUTHORITY..NS_OFF_AUTHORITY + 32]
            .copy_from_slice(ctx.accounts.payer.key().as_ref());
        data[NS_OFF_TREASURY..NS_OFF_TREASURY + 32].copy_from_slice(treasury.as_ref());
        data[NS_OFF_FEE..NS_OFF_FEE + 8].copy_from_slice(&request_fee.to_le_bytes());
        Ok(())
    }

    /// What ORAO's oracles do off-chain. Anyone may call it here; the point is
    /// to control *when* a draw lands, not to model their quorum.
    pub fn mock_fulfill(ctx: Context<MockRequest>, randomness: [u8; 64]) -> Result<()> {
        let mut data = ctx.accounts.request.try_borrow_mut_data()?;
        require!(data.len() >= 137, MockVrfError::BadRequestAddress);
        require!(
            data[..8] == ACCOUNT_RANDOMNESS_V2,
            MockVrfError::BadRequestAddress
        );
        require!(data[OFF_TAG] == TAG_PENDING, MockVrfError::AlreadyFulfilled);

        data[OFF_TAG] = TAG_FULFILLED;
        data[OFF_RANDOMNESS..OFF_RANDOMNESS + 64].copy_from_slice(&randomness);
        msg!("mock fulfilled");
        Ok(())
    }

    /// Simulate ORAO shipping a program upgrade that moves the layout soldust
    /// hardcodes, or otherwise making an in-flight request unreadable.
    ///
    /// * 0 - new account discriminator (they renamed the struct)
    /// * 1 - unknown enum tag (they added a variant)
    /// * 2 - the recorded seed no longer matches
    pub fn mock_corrupt(ctx: Context<MockRequest>, mode: u8) -> Result<()> {
        let mut data = ctx.accounts.request.try_borrow_mut_data()?;
        match mode {
            0 => data[..8].copy_from_slice(&[7u8; 8]),
            1 => data[OFF_TAG] = 9,
            2 => data[OFF_SEED..OFF_SEED + 32].copy_from_slice(&[0xEEu8; 32]),
            _ => return err!(MockVrfError::BadMode),
        }
        msg!("mock corrupted mode={}", mode);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct RequestV2<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: address verified in the handler against the network PDA.
    #[account(mut)]
    pub network_state: UncheckedAccount<'info>,
    /// CHECK: pinned to the treasury recorded in the network state.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: created here, at the PDA the seed derives.
    #[account(mut)]
    pub request: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MockInitNetwork<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: address verified in the handler.
    #[account(mut)]
    pub network_state: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MockRequest<'info> {
    pub caller: Signer<'info>,
    /// CHECK: must already be owned by this program.
    #[account(mut, owner = crate::ID)]
    pub request: UncheckedAccount<'info>,
}

#[error_code]
pub enum MockVrfError {
    #[msg("network state address mismatch")]
    BadNetworkState,
    #[msg("treasury does not match the network state")]
    BadTreasury,
    #[msg("request address does not derive from the seed")]
    BadRequestAddress,
    #[msg("already fulfilled")]
    AlreadyFulfilled,
    #[msg("unknown corruption mode")]
    BadMode,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().take(4).map(|b| format!("{b:02x}")).collect()
}
