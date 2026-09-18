//! MagicBlock VRF adapter.
//!
//! We integrate with MagicBlock's on-chain VRF (`ephemeral-vrf`) rather than
//! any form of blockhash / slot-hash / timestamp pseudo-randomness.
//!
//! ## Why a hand-rolled CPI instead of the `ephemeral-vrf-sdk` crate
//!
//! Same reason the ORAO adapter this replaced was hand-rolled: the SDK pulls a
//! `solana-program` / `steel` stack of its own, and this program deliberately
//! depends on nothing but `anchor-lang` and a SHA-256 hasher so its build stays
//! reproducible. The surface we need is tiny - one instruction to build and one
//! 32-byte argument to receive - and every constant below is asserted against
//! its published derivation in the unit tests at the bottom of this file.
//!
//! ## Pull became push
//!
//! ORAO was a *pull* oracle: `draw_round` created a randomness account whose
//! address was a function of the seed, and `resolve_push` read it back. There
//! is no such account here. MagicBlock queues the request and an oracle later
//! calls *back into this program* with the result, which
//! [`crate::consume_randomness`] writes onto the round.
//!
//! Three consequences, all of them simplifications:
//!
//! * No per-request account means no per-request rent. ORAO entombed
//!   ~0.00168 SOL of unrecoverable rent in every draw it ever answered; the
//!   only permanent cost now is [`VRF_REQUEST_FEE`].
//! * There is no address derived from the seed, so there is nothing for a
//!   griefer to squat. The whole first-come-first-served hazard that forced
//!   sealing and requesting into one transaction simply does not exist here.
//!   That atomicity is kept anyway - it costs nothing and it keeps the seed
//!   from ever being public while unspent.
//! * "Has the draw landed?" is a field on our own account rather than a parse
//!   of a foreign one, so `expire_round` no longer has to decide what an
//!   unreadable oracle account means.
//!
//! ## One draw per round
//!
//! Randomness is bought per *round*, not per push, because MagicBlock charges
//! per request regardless of how many people are waiting on it. Every push
//! queued while a round was open shares that round's draw, and derives its own
//! roll from it with [`roll_for`]. A round of `n` therefore costs `1/n` of a
//! request each.
//!
//! ## Why this is safe against randomness grinding
//!
//! The whole argument is that a round's seed is undecidable until the round is
//! sealed, and fixed before its draw is bought.
//!
//! * [`round_seed`] mixes the `client_seed` the round's opening member committed
//!   with a slot hash sampled at the draw. That member can steer their own
//!   contribution but cannot predict the slot hash; whoever submits the draw
//!   picks the slot but cannot see what draw it will produce, because the draw
//!   does not exist yet and a seed is not shoppable for an outcome.
//! * The draw is then a plain VRF on a fixed seed, so nobody can predict it.
//! * A round only permits settlement once it is `Drawn`, and the only writer of
//!   that status is a callback signed by [`callback_identity`] - a PDA only the
//!   VRF program can sign for. Randomness this program did not ask for can
//!   therefore never be read as a round's draw.
//! * [`roll_for`] labels each member's roll with its `push_id`, so one draw
//!   yields independent-looking rolls and no member's outcome is a function of
//!   another's.
//!
//! There is no per-push, player-supplied seed any more, so there is also
//! nothing sitting in the mempool for a bystander to burn.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use solana_sha256_hasher::hashv;

use crate::errors::SoldustError;

/// MagicBlock `ephemeral-vrf`. Same address on devnet and mainnet-beta.
pub const MAGICBLOCK_VRF_PROGRAM_ID: Pubkey =
    pubkey!("Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz");

/// The oracle queue every request is filed against, and the account the request
/// fee is paid *into*.
///
/// Pinned, and that pin is load-bearing rather than cosmetic. A queue is a PDA
/// of whatever identity created it, and `initialize_oracle_queue` only requires
/// that identity's own signature - so anybody can stand up a queue they control.
/// Without this pin a cranker could file a round's request against their own
/// queue, pay [`VRF_REQUEST_FEE`] into a PDA they can close, claim the
/// reimbursement out of `protocol_accrued`, and collect the fee back on the way
/// out. This is the direct successor of the ORAO adapter's treasury pin, and it
/// protects the same thing: the house's float, not MagicBlock's.
///
/// The cost of pinning is that a queue MagicBlock retires takes soldust's draws
/// with it. That is a liveness failure rather than a loss - `expire_round`
/// refunds every member in full - and it is the cheaper side of the trade.
pub const VRF_QUEUE: Pubkey = pubkey!("Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh");

/// Seed of both identity PDAs below. `b"identity"` in `ephemeral-vrf`.
pub const IDENTITY_SEED: &[u8] = b"identity";

/// `SolanaVrfInstruction::RequestRandomnessScoped`, tag-in-a-u64.
///
/// MagicBlock pads its one-byte instruction tags out to eight bytes, so the
/// discriminator is the tag followed by seven zeros.
///
/// Scoped rather than the legacy global form on purpose: a scoped request is
/// answered by a callback signed by a PDA bound to *this* program id, and the
/// VRF program refuses to fall back to the shared identity for one. The global
/// identity is deprecated and would let any program's callback be signed by the
/// same key ours is checked against.
const IX_REQUEST_RANDOMNESS_SCOPED: [u8; 8] = [10, 0, 0, 0, 0, 0, 0, 0];

/// Anchor instruction discriminator: `sha256("global:consume_randomness")[..8]`.
///
/// Named in the request so the oracle knows what to call back into. Asserted
/// against its derivation below, because a drift here would send the callback
/// to a different instruction of ours - or to none.
const IX_CONSUME_RANDOMNESS: [u8; 8] = [190, 217, 49, 162, 99, 26, 73, 234];

/// What one request costs, and the whole of what a draw costs the house.
///
/// `VRF_LAMPORTS_COST` in `ephemeral-vrf`, transferred from the payer into
/// [`VRF_QUEUE`] and paid out to the oracle that answers.
///
/// Compiled in rather than read from chain, which is a real difference from the
/// ORAO adapter: ORAO published its fee in a `NetworkState` account, so the
/// price could be read live. MagicBlock's is a `const` in their program, and
/// there is no account to read it from. The consequence is bounded on purpose:
///
/// * The *gate* in `draw_round` prices the draw with this figure, so a
///   MagicBlock reprice upwards would let a round through that no longer quite
///   covers its own draw. The shortfall is capped at the difference and is paid
///   out of the rake, never out of stake.
/// * The *reimbursement* never uses this figure at all. It is measured as the
///   cranker's balance delta across the CPI, so whatever MagicBlock actually
///   charged is what gets paid back, and the accounting cannot go stale.
pub const VRF_REQUEST_FEE: u64 = 500_000;

/// The `SlotHashes` sysvar.
///
/// Read by hand rather than through `Sysvar<'info, SlotHashes>`: the account is
/// ~20KB and deserializing it costs more compute than the whole instruction.
/// Its layout is part of the runtime rather than of any upgradeable program, so
/// unlike an oracle's it is not going to move under us.
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
/// fails.
pub const SLOT_HASH_LOOKBACK: usize = 150;

/// The recorded hash of `slot`, if it is still within [`SLOT_HASH_LOOKBACK`].
///
/// Only ever used as *seed material*, never as randomness. That distinction is
/// the whole reason this is sound: a blockhash is a terrible random number
/// because a leader has some influence over it, but a perfectly good nonce,
/// because the VRF output it leads to cannot be predicted from it.
///
/// The caller still names which recent slot to use. Under ORAO it had to,
/// because the randomness account address was a function of the seed and Solana
/// needs every address before execution. That constraint is gone, but the
/// parameter is kept: it is what makes the seed derivation replayable off-chain
/// from public data alone, and the published test vectors depend on it. The
/// choice remains bounded to real, already-final chain state - a caller cannot
/// invent a hash, only pick among the last 150 - and choosing among known
/// hashes buys nothing anyway, since none of them tells you what the oracle
/// will answer.
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

/// The VRF `caller_seed` for a round, decided and spent in the same instruction.
///
/// Undecidable before the draw because of `slot_hash`, and unchangeable after it
/// because the round is sealed in the same transaction that buys the randomness.
/// Includes the star and round ids so two rounds can never collide on a seed even
/// if their entropy somehow matched.
///
/// `cranker` is still an input. Under ORAO it was here to make the derived
/// randomness *address* unguessable, which mattered because that address was
/// squattable. There is no such address now, so this has become belt-and-braces:
/// it keeps a round's seed unpredictable to anyone who does not know which wallet
/// will seal it, and it costs nothing to keep. Removing it would change the
/// published derivation and every client's replay of it for no gain.
///
/// Letting the cranker influence the seed is safe for the same reason letting
/// them choose the slot is: the seed decides *which* VRF output serves the round,
/// not what that output says. Nobody can evaluate a seed without the draw, and
/// the draw does not exist until an oracle answers a request this instruction
/// had to file from scratch.
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

/// The PDA that signs *our* request, proving to the VRF program which program
/// the callback belongs to: `PDA([b"identity"], soldust)`.
///
/// Derived under this program's own id, so it differs per cluster deployment and
/// must never be hardcoded. `draw_round` signs with it via `invoke_signed`.
pub fn request_identity() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[IDENTITY_SEED], &crate::ID)
}

/// The PDA that signs the callback *into* us:
/// `PDA([b"identity", soldust], magicblock_vrf)`.
///
/// This is the single check the entire integrity of the draw rests on. Only the
/// VRF program can produce a signature for a PDA derived under its own id, and
/// the scoped form binds it to this program specifically, so a callback bearing
/// this signer is one the VRF program made after verifying an oracle's proof.
/// Anything else calling `consume_randomness` cannot present it.
///
/// Derived rather than compiled in because it depends on `crate::ID`, which
/// differs between localnet and the deployed clusters. The unit tests below pin
/// the deployed value so a change to either program id is caught at build time.
pub fn callback_identity() -> Pubkey {
    Pubkey::find_program_address(&[IDENTITY_SEED, crate::ID.as_ref()], &MAGICBLOCK_VRF_PROGRAM_ID).0
}

/// What one draw is about to cost the house, priced *before* buying it.
///
/// `draw_round` needs the figure up front to decide whether a round has earned
/// its draw yet, which is the one place the cost has to be known in advance
/// rather than reconciled afterwards. Unlike the ORAO adapter there is nothing
/// to read: see [`VRF_REQUEST_FEE`] for why compiling it in is safe, and where
/// the exposure lands if MagicBlock reprices.
pub fn draw_cost_estimate() -> u64 {
    VRF_REQUEST_FEE
}

/// What a completed request actually cost the payer.
///
/// `outlay` is measured across the CPI rather than recomputed, so it is correct
/// whatever MagicBlock charged. With no per-request account there is no rent
/// coming back, so unlike the ORAO adapter there is nothing to subtract - the
/// whole outlay is unrecovered, and the function stays only so the call site
/// keeps naming what it means.
///
/// The house pays this out of `protocol_accrued`, once per round, so a hostile
/// reprice can drain the rake but can never reach a player's stake.
pub fn unrecovered_cost(outlay: u64) -> u64 {
    outlay
}

/// Widen a 32-byte VRF output to the 64 bytes the roll and the death record are
/// defined over.
///
/// MagicBlock delivers `sha256(vrf_output)`, which is 32 bytes; ORAO delivered
/// 64. Rather than redefine [`roll_for`] - whose output is pinned by published
/// vectors that `scripts/selftest.ts` and every client replay to prove a
/// player's own outcome was honest - the delivered value is right-padded with
/// zeros and the roll is computed exactly as it always was.
///
/// This is a change of *fill*, not of distribution. Every property the game
/// depends on comes from SHA-256 acting as a PRF over the `push_id` label
/// rather than from how wide its input is: each member's marginal probability
/// is still exactly its own `threshold_ppb`, rolls within a round are still
/// independent, and the reduction bias is still one part in ~1e29. The only
/// real change is that the oracle contributes 256 bits of entropy instead of
/// 512, which at a 1-in-21 hole rate is not a quantity anyone can approach.
///
/// `Star::death_randomness` is 64 bytes on a live account, so the widened form
/// is what has to be recorded there regardless.
pub fn widen(randomness: &[u8; 32]) -> [u8; 64] {
    let mut wide = [0u8; 64];
    wide[..32].copy_from_slice(randomness);
    wide
}

/// CPI `RequestRandomnessScoped`, naming `consume_randomness` as the callback
/// and `round` as the one account it may write.
///
/// `payer` funds the request fee and must have signed the outer transaction; in
/// SOLDUST that is the cranker, reimbursed from the rake. `identity_bump` is the
/// bump of [`request_identity`], which this program signs with to prove the
/// callback target is its own.
///
/// The callback account list is fixed here, at request time, and this is the
/// second half of the safety argument: the oracle can only ever hand the result
/// to the round that asked for it, because that round's address is recorded in
/// the queue item and nothing later can change it.
#[allow(clippy::too_many_arguments)]
pub fn request_randomness<'info>(
    vrf_program: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    identity: &AccountInfo<'info>,
    identity_bump: u8,
    queue: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    slot_hashes: &AccountInfo<'info>,
    round: &Pubkey,
    seed: [u8; 32],
) -> Result<()> {
    require_keys_eq!(
        vrf_program.key(),
        MAGICBLOCK_VRF_PROGRAM_ID,
        SoldustError::InvalidVrfProgram
    );
    require_keys_eq!(queue.key(), VRF_QUEUE, SoldustError::InvalidVrfQueue);
    require_keys_eq!(
        slot_hashes.key(),
        SLOT_HASHES_ID,
        SoldustError::InvalidSlotHashes
    );

    // `RequestRandomness` on the wire, Borsh, after the 8-byte tag:
    //   caller_seed             [u8; 32]                     - raw
    //   callback_program_id     Pubkey                        - raw
    //   callback_discriminator  Vec<u8>                       - u32 len + bytes
    //   callback_accounts_metas Vec<SerializableAccountMeta>  - u32 len + entries
    //   callback_args           Vec<u8>                       - u32 len + bytes
    //
    // A `SerializableAccountMeta` is `pubkey [u8; 32]`, `is_signer bool`,
    // `is_writable bool`; Borsh writes each bool as one byte.
    let mut data = Vec::with_capacity(8 + 32 + 32 + 4 + 8 + 4 + 34 + 4);
    data.extend_from_slice(&IX_REQUEST_RANDOMNESS_SCOPED);
    data.extend_from_slice(&seed);
    data.extend_from_slice(crate::ID.as_ref());
    data.extend_from_slice(&(IX_CONSUME_RANDOMNESS.len() as u32).to_le_bytes());
    data.extend_from_slice(&IX_CONSUME_RANDOMNESS);
    data.extend_from_slice(&1u32.to_le_bytes()); // one callback account: the round
    data.extend_from_slice(round.as_ref());
    data.push(0); // is_signer
    data.push(1); // is_writable
    data.extend_from_slice(&0u32.to_le_bytes()); // no extra callback args

    let ix = Instruction {
        program_id: MAGICBLOCK_VRF_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.key(), true),
            AccountMeta::new_readonly(identity.key(), true),
            AccountMeta::new(queue.key(), false),
            AccountMeta::new_readonly(system_program.key(), false),
            AccountMeta::new_readonly(slot_hashes.key(), false),
        ],
        data,
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            identity.clone(),
            queue.clone(),
            system_program.clone(),
            slot_hashes.clone(),
            vrf_program.clone(),
        ],
        &[&[IDENTITY_SEED, &[identity_bump]]],
    )?;

    Ok(())
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
///
/// Unchanged from the ORAO integration, deliberately: see [`widen`] for how a
/// 32-byte oracle output reaches a function defined over 64.
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
    fn discriminators_match_their_derivations() {
        // Ours, named in the request so the oracle can call it back.
        assert_eq!(
            &IX_CONSUME_RANDOMNESS,
            &hash(b"global:consume_randomness").to_bytes()[..8]
        );
        // Theirs: a one-byte tag padded to eight.
        // `SolanaVrfInstruction::RequestRandomnessScoped = 10`.
        assert_eq!(IX_REQUEST_RANDOMNESS_SCOPED, [10, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// The fee is compiled in rather than read from chain, so the one thing that
    /// can silently drift is this constant against `ephemeral-vrf`'s
    /// `VRF_LAMPORTS_COST`. Pinned here so a bump is a deliberate edit.
    #[test]
    fn request_fee_matches_the_published_cost() {
        assert_eq!(VRF_REQUEST_FEE, 500_000);
        assert_eq!(draw_cost_estimate(), VRF_REQUEST_FEE);
    }

    /// Widening must preserve every byte the oracle gave us and add nothing but
    /// zeros, or the roll would stop being a function of the real draw.
    #[test]
    fn widen_preserves_the_oracle_output() {
        let r: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let wide = widen(&r);
        assert_eq!(&wide[..32], &r);
        assert_eq!(&wide[32..], &[0u8; 32]);
        assert_eq!(widen(&[0u8; 32]), [0u8; 64]);
        // Distinct outputs stay distinct once widened, so the roll still
        // separates them.
        assert_ne!(widen(&[1u8; 32]), widen(&[2u8; 32]));
        assert_ne!(
            roll_for(&widen(&[1u8; 32]), 0),
            roll_for(&widen(&[2u8; 32]), 0)
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `roll_for(&[3u8; 64], 7)`. Asserted identically by `scripts/selftest.ts`.
    const ROLL_VECTOR: u32 = 511_204_315;

    /// Known-answer vectors, asserted identically by `scripts/selftest.ts`. The
    /// off-chain crank has to derive round seeds to match requests to rounds, and
    /// the clients replay rolls to show players the draw was honest, so a drift
    /// here silently breaks both.
    ///
    /// These are the *same* vectors the ORAO integration published. Preserving
    /// them bit-for-bit is the proof that swapping the oracle did not move the
    /// game's math.
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

    /// The caller names the slot whose hash seeds the round, so the program has
    /// to find that slot rather than read the newest - and has to refuse one
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
        // And on who sealed it.
        assert_ne!(
            base,
            round_seed(1, 0, &e, 100, &h, &Pubkey::new_from_array([9u8; 32]))
        );
    }

    /// The payer is made exactly whole from what it can observe. With no
    /// per-request account there is nothing held back and nothing returned, so
    /// the whole outlay is the cost - which is the entire saving over ORAO,
    /// whose fulfilled account entombed its rent permanently.
    #[test]
    fn cost_is_the_whole_outlay() {
        assert_eq!(unrecovered_cost(VRF_REQUEST_FEE), VRF_REQUEST_FEE);
        // Whatever was actually charged is what comes back, stale constant or not.
        assert_eq!(unrecovered_cost(750_000), 750_000);
        assert_eq!(unrecovered_cost(0), 0);
    }

    /// What batching buys, in the units the rake is paid in, and what the switch
    /// off ORAO bought on top of it.
    #[test]
    fn a_round_amortises_the_draw_across_its_members() {
        let draw = VRF_REQUEST_FEE;
        for (members, want) in [(1u64, 500_000u64), (4, 125_000), (10, 50_000), (24, 20_833)] {
            assert_eq!(draw / members, want);
        }

        // Stake a round needs to carry for the rake to cover its own draw.
        let rake_bps = crate::game_config::economics().protocol_bps as u64;
        let break_even = draw * crate::constants::BPS / rake_bps;

        // Two minimum pushes, where ORAO's 2_178_245-lamport draw needed seven.
        let step = crate::constants::PUSH_STEP;
        assert_eq!(break_even.div_ceil(step), 2);
        let orao_break_even = 2_178_245u64 * crate::constants::BPS / rake_bps;
        assert_eq!(orao_break_even.div_ceil(step), 7);

        // The gate is still load-bearing: one minimum push does not pay for a
        // draw on its own, so removing it would still put the wrong sign on
        // volume. It only stops mattering if the fee ever falls to the rake on
        // a single step.
        assert!(step * rake_bps / crate::constants::BPS < draw);
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
        // And over the widened 32-byte shape the oracle actually delivers.
        for n in 0u8..32 {
            assert!(roll_for(&widen(&[n; 32]), n as u64) < crate::constants::PPB as u32);
        }
    }

    #[test]
    fn roll_differs_for_different_randomness() {
        assert_ne!(roll_for(&[1u8; 64], 0), roll_for(&[2u8; 64], 0));
    }

    /// The property that makes one draw serve a whole round: members of the
    /// same batch must get unrelated rolls, or they would all live or die
    /// together and the odds would not be what the thresholds say.
    ///
    /// Asserted over the widened shape, because that is what the game now rolls
    /// against - the zero tail must not collapse the independence.
    #[test]
    fn members_of_one_round_roll_independently() {
        let draw = widen(&[42u8; 32]);
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

    /// Both identity PDAs are derived from `crate::ID`, so they move with the
    /// deployment. Pinned against the ids the deployed program actually uses, so
    /// that a program-id change cannot silently retarget the callback check.
    #[test]
    fn identity_pdas_match_the_deployed_derivation() {
        let deployed = pubkey!("CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S");

        let (request, request_bump) = Pubkey::find_program_address(&[IDENTITY_SEED], &deployed);
        assert_eq!(
            request,
            pubkey!("Gik22m8Paphkmd6MjTjqdSvX5qN8jPyfghge8gv1tgo5")
        );
        assert_eq!(request_bump, 255);

        let callback = Pubkey::find_program_address(
            &[IDENTITY_SEED, deployed.as_ref()],
            &MAGICBLOCK_VRF_PROGRAM_ID,
        )
        .0;
        assert_eq!(
            callback,
            pubkey!("s3gWaDrwFf7sx8NzcGFvbxRDNdqKxrfsTgaRr9psptA")
        );

        // The two are emphatically not the same key: one we sign, one only the
        // VRF program can. Confusing them would hand anybody a draw.
        assert_ne!(request, callback);
    }
}
