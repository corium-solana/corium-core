//! ORAO VRF adapter.
//!
//! We integrate with ORAO's on-chain VRF (`request_v2` / `fulfill_v2`) rather
//! than any form of blockhash / slot-hash / timestamp pseudo-randomness.
//!
//! ## Why a hand-rolled CPI instead of the `orao-solana-vrf` crate
//!
//! `orao-solana-vrf` 0.7 pins `anchor-lang ^0.32.1`, which cannot coexist with
//! the Anchor 1.x this program is built on. Rather than pin the whole program
//! to an older Anchor, we build the one instruction we need by hand. The
//! surface is tiny: a 40-byte instruction and a read-only account parse. Both
//! are covered by unit tests against constants derived from ORAO's published
//! source (crate v0.7.0).
//!
//! ## One draw per round
//!
//! Randomness is bought per *round*, not per push, because ORAO charges per
//! request regardless of how many people are waiting on it. Every push queued
//! while a round was open shares that round's draw, and derives its own roll
//! from it with [`roll_for`]. A round of `n` therefore costs `1/n` of a
//! request each.
//!
//! ## Why this is safe against randomness grinding
//!
//! The whole argument is that a round's seed is undecidable until the round is
//! sealed against new members, and fixed before its draw is bought.
//!
//! * [`round_seed`] mixes the `client_seed` the round's opening member committed
//!   with a slot hash sampled at the draw. That member can steer their own
//!   contribution but cannot predict the slot hash; whoever submits the draw
//!   picks the slot but cannot see what draw it will produce, because the draw
//!   does not exist yet and a seed is not shoppable for an outcome.
//! * The draw is then a plain VRF on a fixed seed, so nobody can predict it.
//! * A round only permits settlement once it is `Requested`, so a randomness
//!   account created by anyone other than `request_round_vrf` can never be read
//!   as a round's draw even if its seed somehow collided.
//! * [`roll_for`] labels each member's roll with its `push_id`, so one draw
//!   yields independent-looking rolls and no member's outcome is a function of
//!   another's.
//!
//! There is no per-push, player-supplied seed any more, so there is also
//! nothing sitting in the mempool for a bystander to burn.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke,
};
use solana_sha256_hasher::hashv;

use crate::errors::SoldustError;

/// ORAO VRF v2. Same address on devnet and mainnet-beta.
pub const ORAO_VRF_PROGRAM_ID: Pubkey = pubkey!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");

pub const ORAO_RANDOMNESS_SEED: &[u8] = b"orao-vrf-randomness-request";
pub const ORAO_NETWORK_SEED: &[u8] = b"orao-vrf-network-configuration";

/// Anchor instruction discriminator: `sha256("global:request_v2")[..8]`.
const IX_REQUEST_V2: [u8; 8] = [38, 151, 209, 6, 195, 102, 28, 217];

/// Anchor account discriminator: `sha256("account:RandomnessV2")[..8]`.
const ACCOUNT_RANDOMNESS_V2: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];

/// Borsh enum tags of `RequestAccount`.
const TAG_PENDING: u8 = 0;
const TAG_FULFILLED: u8 = 1;

// `RandomnessV2 { request: RequestAccount::Fulfilled(FulfilledRequest) }` on the wire:
//   [0..8)     account discriminator
//   [8]        enum tag (1 = Fulfilled)
//   [9..41)    client:     Pubkey
//   [41..73)   seed:       [u8; 32]
//   [73..137)  randomness: [u8; 64]
const OFF_TAG: usize = 8;
const OFF_SEED: usize = 41;
const OFF_RANDOMNESS: usize = 73;
const FULFILLED_LEN: usize = 137;

/// Byte length of a fulfilled request. ORAO creates the account much larger
/// (it has room for every oracle's response), then shrinks it to this on
/// fulfill and returns the freed rent to whoever paid for the request.
pub const FULFILLED_SIZE: usize = FULFILLED_LEN;

/// The `SlotHashes` sysvar.
///
/// Read by hand rather than through `Sysvar<'info, SlotHashes>`: the account is
/// ~20KB and deserializing it costs more compute than the whole instruction.
/// Its layout is part of the runtime rather than of any upgradeable program, so
/// unlike ORAO's it is not going to move under us.
pub const SLOT_HASHES_ID: Pubkey = pubkey!("SysvarS1otHashes111111111111111111111111111");

// `SlotHashes` on the wire: a Borsh vec of `(Slot, Hash)`, most recent first.
//   [0..8)    entry count: u64
//   [8..16)   entry 0 slot: u64
//   [16..48)  entry 0 hash: [u8; 32]
const SLOT_HASHES_COUNT_LEN: usize = 8;
const SLOT_HASHES_ENTRY_LEN: usize = 40;

/// How far back a caller may reach into `SlotHashes` for seed material.
///
/// Chosen to match the 150-slot lifetime of a transaction's blockhash, so this
/// check can never be the reason a transaction that was otherwise still valid
/// fails. The caller has to name its slot up front - the ORAO account address
/// depends on it, and Solana needs every address before execution - so anything
/// tighter would turn ordinary network delay into a spurious failure.
pub const SLOT_HASH_LOOKBACK: usize = 150;

/// The recorded hash of `slot`, if it is still within [`SLOT_HASH_LOOKBACK`].
///
/// Only ever used as *seed material*, never as randomness. That distinction is
/// the whole reason this is sound: a blockhash is a terrible random number
/// because a leader has some influence over it, but a perfectly good nonce,
/// because the VRF output it leads to cannot be predicted from it.
///
/// The caller chooses which recent slot to use, because it has to derive the
/// resulting ORAO address before it can even build the transaction. That choice
/// is bounded to real, already-final chain state - a caller cannot invent a hash,
/// only pick among the last 150 - and choosing among known hashes buys nothing
/// anyway: none of them tells you what ORAO will answer.
pub fn slot_hash_at(slot_hashes: &AccountInfo, slot: u64) -> Result<[u8; 32]> {
    require_keys_eq!(
        slot_hashes.key(),
        SLOT_HASHES_ID,
        SoldustError::InvalidSlotHashes
    );
    let data = slot_hashes.try_borrow_data()?;
    find_slot_hash(&data, slot)
}

/// The scan itself, over the sysvar's raw bytes.
fn find_slot_hash(data: &[u8], slot: u64) -> Result<[u8; 32]> {
    require!(
        data.len() >= SLOT_HASHES_COUNT_LEN,
        SoldustError::InvalidSlotHashes
    );
    let mut count = [0u8; 8];
    count.copy_from_slice(&data[..SLOT_HASHES_COUNT_LEN]);
    let count = u64::from_le_bytes(count) as usize;
    require!(count > 0, SoldustError::InvalidSlotHashes);

    // Entries run most-recent-first and slots can be skipped, so there is no
    // arithmetic shortcut to the index; scan, bounded by the lookback.
    let entries = count.min(SLOT_HASH_LOOKBACK);
    require!(
        data.len() >= SLOT_HASHES_COUNT_LEN + entries * SLOT_HASHES_ENTRY_LEN,
        SoldustError::InvalidSlotHashes
    );
    for i in 0..entries {
        let at = SLOT_HASHES_COUNT_LEN + i * SLOT_HASHES_ENTRY_LEN;
        let mut candidate = [0u8; 8];
        candidate.copy_from_slice(&data[at..at + 8]);
        if u64::from_le_bytes(candidate) == slot {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data[at + 8..at + 40]);
            return Ok(hash);
        }
    }
    err!(SoldustError::SlotHashTooOld)
}

/// Commit the player-supplied half of a round's seed. Called once per round, by
/// whichever push opened it.
///
/// A player picks their own `client_seed`, so they can drive this to any value
/// they like - that is fine and expected. Its job is not to be unpredictable on
/// its own but to stop the draw-time slot hash from being the *sole* input, so
/// that neither a member nor a block leader has unilateral control of the seed.
///
/// It takes the previous value so the derivation stays a fold and the published
/// test vectors keep meaning what they meant, but on chain the previous value is
/// always zero: a round's entropy is written when `member_count` is 0 and left
/// alone after that. See [`crate::state::Round::entropy`] for why it must not
/// move once a draw can be attempted against it.
pub fn fold_entropy(
    entropy: &[u8; 32],
    client_seed: &[u8; 32],
    player: &Pubkey,
    push_id: u64,
) -> [u8; 32] {
    hashv(&[
        b"soldust:entropy",
        entropy,
        client_seed,
        player.as_ref(),
        &push_id.to_le_bytes(),
    ])
    .to_bytes()
}

/// The ORAO seed for a round, decided and spent in the same instruction.
///
/// Undecidable before the draw because of `slot_hash`, and unchangeable after it
/// because the round is sealed in the same transaction that buys the randomness.
/// Includes the star and round ids so two rounds can never collide on a seed even
/// if their entropy somehow matched.
///
/// `cranker` is in here to make the resulting ORAO address harder to predict for
/// anyone who wants to occupy it first. Within a slot the slot hash is already
/// public, so without this a griefer watching a known crank wallet could compute
/// the address it is about to request. With it, they must also know which wallet
/// will sign - so rotating crank wallets defeats them outright, and even a
/// correct guess only costs the crank a reverted transaction, because the seal
/// and the purchase are atomic and the next attempt samples a fresh slot hash.
///
/// Letting the cranker influence the seed is safe for the same reason letting
/// them choose the slot is: the seed decides *which* VRF output serves the round,
/// not what that output says. Nobody can evaluate a seed without the draw, and
/// the draw does not exist until ORAO answers a request that this instruction has
/// to create from scratch - an address that already holds a known answer cannot
/// be adopted, it just fails.
pub fn round_seed(
    star_id: u64,
    round_id: u64,
    entropy: &[u8; 32],
    slot: u64,
    slot_hash: &[u8; 32],
    cranker: &Pubkey,
) -> [u8; 32] {
    hashv(&[
        b"soldust:round",
        &star_id.to_le_bytes(),
        &round_id.to_le_bytes(),
        entropy,
        &slot.to_le_bytes(),
        slot_hash,
        cranker.as_ref(),
    ])
    .to_bytes()
}

/// Address of the randomness account for `seed`.
pub fn randomness_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[ORAO_RANDOMNESS_SEED, seed], &ORAO_VRF_PROGRAM_ID).0
}

/// Address of ORAO's network configuration account.
pub fn network_state_address() -> Pubkey {
    Pubkey::find_program_address(&[ORAO_NETWORK_SEED], &ORAO_VRF_PROGRAM_ID).0
}

// `NetworkState` on the wire:
//   [0..8)    account discriminator
//   [8..40)   authority:   Pubkey
//   [40..72)  treasury:    Pubkey
//   [72..80)  request_fee: u64
const OFF_TREASURY: usize = 40;
const OFF_REQUEST_FEE: usize = 72;

/// What one draw is about to cost the house, priced *before* buying it.
///
/// Exactly the two components [`unrecovered_cost`] measures after the fact: the
/// fee ORAO keeps, plus the rent that stays locked in the fulfilled account
/// forever. Both are live state - the fee is ORAO's to change, the rent rate is
/// the runtime's, and the two disagree between clusters - so this is read at
/// call time and never compiled in.
///
/// `close_round` needs the figure up front to decide whether a round has earned
/// its draw yet, which is the one place the cost has to be known in advance
/// rather than reconciled afterwards.
pub fn draw_cost_estimate(network_state: &AccountInfo) -> Result<u64> {
    require_keys_eq!(
        network_state.key(),
        network_state_address(),
        SoldustError::InvalidVrfNetworkState
    );
    let data = network_state.try_borrow_data()?;
    require!(
        data.len() >= OFF_REQUEST_FEE + 8,
        SoldustError::InvalidVrfNetworkState
    );
    let mut fee = [0u8; 8];
    fee.copy_from_slice(&data[OFF_REQUEST_FEE..OFF_REQUEST_FEE + 8]);
    Ok(u64::from_le_bytes(fee).saturating_add(Rent::get()?.minimum_balance(FULFILLED_SIZE)))
}

/// Refuse to pay ORAO's fee into an account ORAO itself does not name.
///
/// ORAO's `request_v2` pins its treasury against this same field, so on today's
/// build this can never fire. It is here because the pin protects *our* float
/// rather than ORAO's: [`unrecovered_cost`] prices the draw as the cranker's own
/// balance delta, so a build of ORAO that stopped checking would let a cranker
/// name a wallet it controls, keep the request fee, and be reimbursed for it out
/// of `protocol_accrued` anyway. ORAO is upgradeable and this program is meant to
/// end up immutable, so the cheaper side of that asymmetry is to check here.
pub fn require_network_treasury(network_state: &AccountInfo, treasury: &Pubkey) -> Result<()> {
    require_keys_eq!(
        network_state.key(),
        network_state_address(),
        SoldustError::InvalidVrfNetworkState
    );
    let data = network_state.try_borrow_data()?;
    require!(
        data.len() >= OFF_TREASURY + 32,
        SoldustError::InvalidVrfNetworkState
    );
    let mut named = [0u8; 32];
    named.copy_from_slice(&data[OFF_TREASURY..OFF_TREASURY + 32]);
    require_keys_eq!(
        Pubkey::new_from_array(named),
        *treasury,
        SoldustError::InvalidVrfTreasury
    );
    Ok(())
}

/// What a completed `request_v2` actually cost the payer, net of the rent
/// ORAO is going to hand back.
///
/// `outlay` is measured across the CPI rather than recomputed, so it is
/// correct whatever ORAO charged - the request fee is a number ORAO can change
/// at will and the rent rate is per-cluster, so neither can be compiled in. The
/// account currently holds its full pending rent; everything above the
/// fulfilled minimum returns to the payer when the oracles answer.
///
/// The house pays this out of `protocol_accrued`, once per round, so a hostile
/// ORAO reprice can drain the rake but can never reach a player's stake.
pub fn unrecovered_cost(outlay: u64, request_account: &AccountInfo) -> Result<u64> {
    Ok(unrecovered(
        outlay,
        request_account.lamports(),
        Rent::get()?.minimum_balance(FULFILLED_SIZE),
    ))
}

fn unrecovered(outlay: u64, held_now: u64, locked_rent: u64) -> u64 {
    outlay.saturating_sub(held_now.saturating_sub(locked_rent))
}

/// CPI `request_v2(seed)`.
///
/// `payer` funds ORAO's request fee and the rent for the randomness account,
/// and must have signed the outer transaction. In SOLDUST that is the player,
/// which is what keeps the whole push to a single signature.
#[allow(clippy::too_many_arguments)]
pub fn request_randomness<'info>(
    vrf_program: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    network_state: &AccountInfo<'info>,
    treasury: &AccountInfo<'info>,
    request: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    seed: [u8; 32],
) -> Result<()> {
    require_keys_eq!(
        vrf_program.key(),
        ORAO_VRF_PROGRAM_ID,
        SoldustError::InvalidVrfProgram
    );
    require_keys_eq!(
        network_state.key(),
        network_state_address(),
        SoldustError::InvalidVrfNetworkState
    );
    require_keys_eq!(
        request.key(),
        randomness_address(&seed),
        SoldustError::RandomnessAccountMismatch
    );

    let mut data = Vec::with_capacity(IX_REQUEST_V2.len() + seed.len());
    data.extend_from_slice(&IX_REQUEST_V2);
    data.extend_from_slice(&seed); // Borsh [u8; 32] is the raw bytes.

    let ix = Instruction {
        program_id: ORAO_VRF_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.key(), true),
            AccountMeta::new(network_state.key(), false),
            AccountMeta::new(treasury.key(), false),
            AccountMeta::new(request.key(), false),
            AccountMeta::new_readonly(system_program.key(), false),
        ],
        data,
    };

    invoke(
        &ix,
        &[
            payer.clone(),
            network_state.clone(),
            treasury.clone(),
            request.clone(),
            system_program.clone(),
            vrf_program.clone(),
        ],
    )?;

    Ok(())
}

/// Read a fulfilled VRF result.
///
/// Returns `Ok(None)` while the request is still pending, so the caller can
/// decide whether that is an error (alive star, must wait) or irrelevant
/// (dead star, refund immediately).
///
/// `expected_seed` is enforced: an account can only be accepted if ORAO
/// recorded it against exactly the seed this push owns.
pub fn read_fulfilled(
    account: &AccountInfo,
    expected_seed: &[u8; 32],
) -> Result<Option<[u8; 64]>> {
    require_keys_eq!(
        *account.owner,
        ORAO_VRF_PROGRAM_ID,
        SoldustError::InvalidVrfAccountOwner
    );

    let data = account.try_borrow_data()?;
    require!(data.len() > OFF_TAG, SoldustError::MalformedVrfAccount);
    require!(
        data[..8] == ACCOUNT_RANDOMNESS_V2,
        SoldustError::MalformedVrfAccount
    );

    match data[OFF_TAG] {
        TAG_PENDING => Ok(None),
        TAG_FULFILLED => {
            require!(
                data.len() >= FULFILLED_LEN,
                SoldustError::MalformedVrfAccount
            );

            let mut seed = [0u8; 32];
            seed.copy_from_slice(&data[OFF_SEED..OFF_SEED + 32]);
            require!(
                seed == *expected_seed,
                SoldustError::RandomnessSeedMismatch
            );

            let mut randomness = [0u8; 64];
            randomness.copy_from_slice(&data[OFF_RANDOMNESS..OFF_RANDOMNESS + 64]);
            Ok(Some(randomness))
        }
        _ => err!(SoldustError::MalformedVrfAccount),
    }
}

/// One member's roll, in `[0, 1_000_000_000)`.
///
/// All 64 bytes of the draw are hashed together with the member's `push_id`, so
/// every push in a round gets its own roll out of the one draw. SHA-256 is a
/// PRF here: the rolls are independent-looking, and knowing one tells you
/// nothing about the others even though they share an input.
///
/// The `push_id` label is what makes sharing a draw safe. Without it every
/// member of a round would roll the same number and a round would live or die
/// as a block, which is a completely different game with completely different
/// odds. With it, each member's marginal probability is still exactly its own
/// `threshold_ppb`, so the return identity and the 1/21 hole rate are untouched.
///
/// Reduction bias is the first 16 bytes taken mod 1e9, i.e. about 1 part in
/// 1e29. That is not a number anyone can exploit.
pub fn roll_for(randomness: &[u8; 64], push_id: u64) -> u32 {
    let h = hashv(&[b"soldust:roll", randomness, &push_id.to_le_bytes()]).to_bytes();
    let mut word = [0u8; 16];
    word.copy_from_slice(&h[..16]);
    (u128::from_le_bytes(word) % crate::constants::PPB as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sha256_hasher::hash;

    #[test]
    fn discriminators_match_anchor_derivation() {
        assert_eq!(&IX_REQUEST_V2, &hash(b"global:request_v2").to_bytes()[..8]);
        assert_eq!(
            &ACCOUNT_RANDOMNESS_V2,
            &hash(b"account:RandomnessV2").to_bytes()[..8]
        );
    }

    /// Both `NetworkState` fields this program reads are addressed by literal
    /// offset, and they are adjacent, so one bad constant would silently read the
    /// other field. The treasury pin depends on this as much as the price does.
    #[test]
    fn network_state_offsets_are_self_consistent() {
        assert_eq!(OFF_TREASURY + 32, OFF_REQUEST_FEE);
    }

    #[test]
    fn fulfilled_layout_offsets_are_self_consistent() {
        // 8 disc + 1 tag + 32 client + 32 seed + 64 randomness
        assert_eq!(OFF_TAG, 8);
        assert_eq!(OFF_SEED, OFF_TAG + 1 + 32);
        assert_eq!(OFF_RANDOMNESS, OFF_SEED + 32);
        assert_eq!(FULFILLED_LEN, OFF_RANDOMNESS + 64);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `roll_for(&[3u8; 64], 7)`. Asserted identically by `scripts/selftest.ts`.
    const ROLL_VECTOR: u32 = 511_204_315;

    /// Known-answer vectors, asserted identically by `scripts/selftest.ts`. The
    /// off-chain crank has to derive round seeds to find ORAO accounts, and the
    /// clients replay rolls to show players the draw was honest, so a drift
    /// here silently breaks both.
    #[test]
    fn round_seed_matches_the_typescript_client() {
        let entropy = fold_entropy(&[0u8; 32], &[1u8; 32], &Pubkey::default(), 0);
        assert_eq!(
            hex(&entropy),
            "5a71a6ac26dc4551af42e4520b103b0de576daf73d8778e71351ca683ded4e2b"
        );
        let seed = round_seed(1, 0, &entropy, 300, &[2u8; 32], &Pubkey::default());
        assert_eq!(
            hex(&seed),
            "c179d64bc131c48529795242fffb4094055a6070f112132e0d05ddcfe94da685"
        );

        // And the roll a member derives from a draw, which the clients replay
        // to show a player their own outcome was not tampered with.
        assert_eq!(roll_for(&[3u8; 64], 7), ROLL_VECTOR);
    }

    /// The fold has to depend on every input it takes, so that no two rounds can
    /// land on the same entropy - a member reusing a `client_seed`, or two members
    /// picking the same one, still gets a different value out.
    #[test]
    fn entropy_depends_on_every_input() {
        let base = fold_entropy(&[0u8; 32], &[1u8; 32], &Pubkey::default(), 0);
        assert_ne!(base, fold_entropy(&[9u8; 32], &[1u8; 32], &Pubkey::default(), 0));
        assert_ne!(base, fold_entropy(&[0u8; 32], &[9u8; 32], &Pubkey::default(), 0));
        assert_ne!(
            base,
            fold_entropy(&[0u8; 32], &[1u8; 32], &Pubkey::new_from_array([9u8; 32]), 0)
        );
        assert_ne!(base, fold_entropy(&[0u8; 32], &[1u8; 32], &Pubkey::default(), 1));
    }

    /// `SlotHashes` bytes for `count` descending slots ending at `newest`, where
    /// entry for slot `s` hashes to `[s as u8; 32]`.
    fn slot_hashes_bytes(newest: u64, count: u64) -> Vec<u8> {
        let mut out = count.to_le_bytes().to_vec();
        for i in 0..count {
            let slot = newest - i;
            out.extend_from_slice(&slot.to_le_bytes());
            out.extend_from_slice(&[slot as u8; 32]);
        }
        out
    }

    /// The caller names the slot whose hash seeds the round, because it has to
    /// derive the ORAO address before it can build the transaction. So the program
    /// has to find that slot rather than read the newest - and has to refuse one
    /// that has aged out, or the bound on what a caller may choose from would be
    /// no bound at all.
    #[test]
    fn a_caller_may_name_any_slot_still_inside_the_lookback() {
        let newest = 10_000u64;
        let data = slot_hashes_bytes(newest, 400);

        assert_eq!(find_slot_hash(&data, newest).unwrap(), [newest as u8; 32]);
        let oldest_allowed = newest - SLOT_HASH_LOOKBACK as u64 + 1;
        assert_eq!(
            find_slot_hash(&data, oldest_allowed).unwrap(),
            [oldest_allowed as u8; 32]
        );

        // Present in the sysvar, but past the lookback: refused all the same.
        assert!(find_slot_hash(&data, oldest_allowed - 1).is_err());
        // Never happened at all, and a future slot cannot have a hash yet.
        assert!(find_slot_hash(&data, newest + 1).is_err());
        // Gaps are normal - skipped slots simply are not there.
        let sparse = {
            let mut out = 2u64.to_le_bytes().to_vec();
            for slot in [500u64, 497] {
                out.extend_from_slice(&slot.to_le_bytes());
                out.extend_from_slice(&[slot as u8; 32]);
            }
            out
        };
        assert!(find_slot_hash(&sparse, 499).is_err());
        assert_eq!(find_slot_hash(&sparse, 497).unwrap(), [497u64 as u8; 32]);

        // And a sysvar that claims more entries than it carries is malformed,
        // not merely a miss - the scan must refuse rather than read past the end.
        assert!(find_slot_hash(&[], 1).is_err());
        assert!(find_slot_hash(&0u64.to_le_bytes(), 1).is_err());
        let short = slot_hashes_bytes(newest, 20);
        assert!(find_slot_hash(&short[..short.len() - 1], newest).is_err());
    }

    /// Two rounds must never share a seed, even given identical entropy and an
    /// identical slot hash - otherwise the second would inherit a draw that was
    /// already public.
    #[test]
    fn round_seeds_are_distinct_across_stars_and_rounds() {
        let e = [7u8; 32];
        let h = [8u8; 32];
        let k = Pubkey::default();
        let base = round_seed(1, 0, &e, 100, &h, &k);
        assert_ne!(base, round_seed(2, 0, &e, 100, &h, &k));
        assert_ne!(base, round_seed(1, 1, &e, 100, &h, &k));
        assert_ne!(base, round_seed(1, 0, &e, 101, &h, &k));
        assert_ne!(base, round_seed(1, 0, &e, 100, &[9u8; 32], &k));
        assert_ne!(base, round_seed(1, 0, &[9u8; 32], 100, &h, &k));
        // And on who sealed it, which is what a griefer would have to guess.
        assert_ne!(
            base,
            round_seed(1, 0, &e, 100, &h, &Pubkey::new_from_array([9u8; 32]))
        );
    }

    /// Real figures, read off both clusters with `yarn measure-orao`. They
    /// disagree with each other on *both* inputs, which is the whole reason
    /// neither is compiled in.
    const DEVNET: (u64, u64, u64) = (300_000, 1_346_200, 4_455_160);
    const MAINNET: (u64, u64, u64) = (500_000, 1_678_245, 5_554_041);

    /// The payer is made exactly whole from what it can observe: what left its
    /// balance, minus what ORAO is holding above the fulfilled minimum and
    /// will therefore hand back.
    #[test]
    fn cost_is_recovered_from_observation_alone() {
        for (fee, locked_rent, pending_rent) in [DEVNET, MAINNET] {
            let outlay = fee + pending_rent;
            assert_eq!(unrecovered(outlay, pending_rent, locked_rent), fee + locked_rent);
        }
        // Cross-check against the numbers traced out of real transactions.
        assert_eq!(unrecovered(4_755_160, 4_455_160, 1_346_200), 1_646_200);
        assert_eq!(unrecovered(6_054_041, 5_554_041, 1_678_245), 2_178_245);
    }

    /// What batching actually buys, in the units the rake is paid in.
    ///
    /// A draw costs the house 2_178_245 lamports on mainnet however many people
    /// are waiting on it, so the honest figure to compare against the rake is
    /// the cost per *member*. At the 314 bps rake a round breaks even once its
    /// members have staked `cost / 0.0314` between them - about 0.07 SOL, which
    /// is seven minimum pushes, or one member of any real size.
    #[test]
    fn a_round_amortises_the_draw_across_its_members() {
        let draw = MAINNET.0 + MAINNET.1;
        assert_eq!(draw, 2_178_245);
        for (members, want) in [(1u64, 2_178_245u64), (4, 544_561), (10, 217_824), (24, 90_760)] {
            assert_eq!(draw / members, want);
        }

        // Stake a round needs to carry for the rake to cover its own draw.
        let rake_bps = crate::game_config::economics().protocol_bps as u64;
        let break_even = draw * crate::constants::BPS / rake_bps;
        assert!(
            (69_000_000..=70_000_000).contains(&break_even),
            "break-even {break_even}"
        );
    }

    #[test]
    fn roll_is_always_in_range() {
        for n in 0u8..64 {
            let mut r = [0u8; 64];
            for (i, b) in r.iter_mut().enumerate() {
                *b = n.wrapping_mul(i as u8).wrapping_add(n);
            }
            assert!(roll_for(&r, n as u64) < crate::constants::PPB as u32);
        }
        assert!(roll_for(&[0u8; 64], 0) < crate::constants::PPB as u32);
        assert!(roll_for(&[0xffu8; 64], u64::MAX) < crate::constants::PPB as u32);
    }

    #[test]
    fn roll_differs_for_different_randomness() {
        assert_ne!(roll_for(&[1u8; 64], 0), roll_for(&[2u8; 64], 0));
    }

    /// The property that makes one draw serve a whole round: members of the
    /// same batch must get unrelated rolls, or they would all live or die
    /// together and the odds would not be what the thresholds say.
    #[test]
    fn members_of_one_round_roll_independently() {
        let draw = [42u8; 64];
        let rolls: Vec<u32> = (0..64u64).map(|id| roll_for(&draw, id)).collect();
        let mut sorted = rolls.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), rolls.len(), "collision within one round");

        // And spread across the range rather than clustered: with 64 draws,
        // every octant of [0, 1e9) should see at least one.
        let octant = crate::constants::PPB as u32 / 8;
        for k in 0..8u32 {
            let lo = k * octant;
            let hi = lo + octant;
            assert!(
                rolls.iter().any(|r| (lo..hi).contains(r)),
                "octant {k} empty"
            );
        }
    }

    /// A member's roll must not move when the round it sits in changes size or
    /// composition. Only the draw and its own id may matter.
    #[test]
    fn a_members_roll_depends_only_on_the_draw_and_its_own_id() {
        let draw = [7u8; 64];
        assert_eq!(roll_for(&draw, 5), roll_for(&draw, 5));
        assert_ne!(roll_for(&draw, 5), roll_for(&draw, 6));
        assert_ne!(roll_for(&draw, 5), roll_for(&[8u8; 64], 5));
    }
}
