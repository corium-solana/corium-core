//! # SOLDUST
//!
//! One playable star at a time. Players push SOL at it; every push that
//! lands grows the star and takes a verifiably random roll at killing it.
//! Whoever lands the lethal push takes the jackpot. Once the current star
//! is finished or has committed its full mass, the next one can be born
//! while the old queue still settles.
//!
//! ## Trust surface
//!
//! There is no admin. No instruction can retune the odds, pause the game,
//! move the treasury, or reach into escrow or prizes. `Config` holds
//! accounting and the treasury address; every number that decides an outcome
//! is compiled into [`game_config`].
//!
//! Exactly one instruction is gated, and only against being run by a stranger
//! *once*: [`initialize`](soldust::initialize) must be signed by the program's
//! BPF upgrade authority, because it is the one call whose effect - naming the
//! treasury - can never be redone. See [`instructions::initialize`].
//!
//! The remaining privileged action in the system is that upgrade authority
//! itself. While it is held, assume an upgrade can do anything; the intent is to
//! set it to `None` once the game has run long enough on mainnet to trust the
//! code without a rescue hatch. Until then it is a real trust assumption and
//! should be read as one.
//!
//! ## Lifecycle
//!
//! ```text
//!   create_first_star / create_next_star
//!            |
//!            v
//!   feed          --mass < 1 SOL, one tx, no VRF-->  hole ticket
//!            |
//!   request_push  --mass >= 1 SOL, escrow stake, join the open Round-->
//!            |
//!            |  draw_round         seal the batch once it is worth a draw and
//!            |                     buy that draw in the same transaction, paid
//!            |                     out of the rake, crank reimbursed
//!            |  consume_randomness  the VRF program calls back with the result
//!            v
//!   resolve_push  (permissionless, once per member, in push_id order)
//!            |
//!            +-- star alive --> settle, roll H(draw, push_id)
//!            +-- star finished --> refund stake, no roll
//!            +-- round expired --> refund stake, no roll, star lives on
//!                                  |
//!                                  v
//!                           claim_prize (killer) / claim_hole_share (early feed)
//!                           create_next_star (anyone; also once committed-full)
//!
//!   collapse_stalled_star  --no new mass for a week, queue empty-->
//!                           feeders refund at cost via claim_hole_share,
//!                           the rest of the pot endows the next star
//! ```
//!
//! ## Randomness is bought per round, not per push
//!
//! The oracle charges per request no matter how many people are waiting on it,
//! so a draw is shared by every push queued while a round was open and each member
//! rolls a labelled hash of it. A round of twenty costs a twentieth of a request
//! each. It is also *faster* than one-request-per-push ever was, because a queue
//! of twenty used to need twenty sequential fulfilments and now needs one.
//!
//! The house pays for it out of `protocol_accrued`, so there is no oracle line
//! item on a player's stake at all. See [`vrf`] for why a shared draw cannot be
//! ground, and [`instructions::round`] for the state machine.
//!
//! ## Nothing can freeze
//!
//! A round that stalls - undrawn, or waiting on an oracle that never answers -
//! can be voided by anyone after
//! [`ROUND_EXPIRY_SLOTS`](constants::ROUND_EXPIRY_SLOTS). Its members refund in
//! full, in `push_id` order like any other resolution, and the star carries on.
//! That matters more than usual here, because mainnet upgrade authority is meant
//! to end up somewhere that cannot ship a rescue patch on demand.
//!
//! Refunds alone are not enough, though, because a refund adds no mass: a star
//! whose oracle never comes back at all would hand every stake back and then sit
//! Alive forever, holding a pot that earlier settles paid in and that now has no
//! claimant - no kill, no Event Horizon - with play unable to advance past it.
//! The same shape appears with nothing broken at all, if people simply stop
//! playing. So
//! [`collapse_stalled_star`](instructions::collapse_stalled) finishes any star
//! that has gone a week without gaining mass (a day if it never left its
//! nursery) and whose queue is empty: feeders take back the prize-side value of
//! their own feeds, the remainder endows the next star, and no lamport leaves the
//! game. It cannot be farmed, because it never pays anyone more than cost, and it
//! cannot cut in front of a live push, because the queue has to be empty first.
//!
//! That is the whole liveness argument, and it holds with the oracle dark and
//! every player gone: voiding a round needs only slots, resolving its members
//! needs only that void, and collapsing the star needs only time. None of the
//! three needs us, and none needs an upgrade.
//!
//! ## Where to change things
//!
//! * Game balance: [`game_config`] - compiled in. There is no retune
//!   instruction. The house edge is 314 bps.
//! * Randomness provider: [`vrf`] - a single module.
//! * Money movement: [`vault`] - the only place lamports leave the program.

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod game_config;
pub mod instructions;
pub mod math;
pub mod state;
pub mod vault;
pub mod vrf;

use instructions::*;

#[cfg(not(feature = "no-entrypoint"))]
use solana_security_txt::security_txt;

declare_id!("CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S");

#[cfg(not(feature = "no-entrypoint"))]
security_txt! {
    name: "CORIUM",
    project_url: "https://www.corium.so",
    contacts: "email:corium.so@proton.me,twitter:@corium_so,link:https://www.corium.so",
    policy: "https://www.corium.so/security.txt",
    preferred_languages: "en",
    auditors: "None"
}

#[program]
pub mod soldust {
    use super::*;

    /// Create `Config` and the vault. Once, and only by the program's BPF upgrade
    /// authority - the one gate in the program, because naming the treasury is
    /// the one decision nothing can revise. No authority is *stored*; the check
    /// is against the loader, and it must happen before upgrade authority is
    /// dropped to `None`.
    ///
    /// * `genesis_seed` - visual seed for star #1. Later stars derive theirs
    ///   from their predecessor's death randomness.
    /// * `treasury` - where `withdraw_protocol_fees` sends protocol revenue.
    pub fn initialize(
        ctx: Context<Initialize>,
        genesis_seed: [u8; 32],
        treasury: Pubkey,
    ) -> Result<()> {
        instructions::initialize::initialize(ctx, genesis_seed, treasury)
    }

    /// Birth star #1. Permissionless, once. Uses `Config.genesis_seed`.
    pub fn create_first_star(ctx: Context<CreateFirstStar>) -> Result<()> {
        instructions::create_star::create_first_star(ctx)
    }

    /// Birth the next star after the current one has died. Permissionless.
    pub fn create_next_star(ctx: Context<CreateNextStar>, new_star_id: u64) -> Result<()> {
        instructions::create_star::create_next_star(ctx, new_star_id)
    }

    /// Nursery hole ticket. One signature, instant settle, no VRF.
    /// Clips to leftover room; fails if the nursery is already full.
    pub fn feed(ctx: Context<Feed>, star_id: u64, amount: u64) -> Result<()> {
        instructions::feed::feed(ctx, star_id, amount)
    }

    /// Last-hit: escrow the stake and join the star's open round. The player's
    /// only signature, and the only lamports it moves are the stake - the draw
    /// this push will roll against is bought later, once, for the whole round,
    /// out of the protocol's own balance.
    ///
    /// * `star_id` - must be the current live star; recorded permanently on
    ///   the push so it can never roll forward to a later star.
    /// * `client_seed` - 32 random bytes from the client. It scopes this push's
    ///   address within the player's own wallet, and if this push is the one that
    ///   opens the round it also becomes that round's entropy. It is *not* a VRF
    ///   seed, so reusing one after `close_push` is harmless.
    pub fn request_push(
        ctx: Context<RequestPush>,
        star_id: u64,
        amount: u64,
        client_seed: [u8; 32],
    ) -> Result<()> {
        instructions::request_push::request_push(ctx, star_id, amount, client_seed)
    }

    /// Seal the star's open round and buy the one draw that serves it, in a
    /// single transaction so the seed is never public while still unspent.
    /// Permissionless; the signer fronts the request fee and is reimbursed from
    /// `protocol_accrued`, capped at what the rake actually holds so the game
    /// cannot stall for lack of funds.
    ///
    /// Allowed once the round's window has elapsed or it is already full enough
    /// that waiting would only add latency - and once the batch's own rake covers
    /// the request fee. A round that has not earned its draw yet stays open and
    /// keeps collecting members.
    ///
    /// * `seed_slot` - a slot within the last `SLOT_HASH_LOOKBACK`, whose recorded
    ///   hash goes into the seed. The caller names it so the derivation stays
    ///   replayable off-chain; the program verifies the slot against `SlotHashes`.
    pub fn draw_round(ctx: Context<DrawRound>, seed_slot: u64) -> Result<()> {
        instructions::round::draw_round(ctx, seed_slot)
    }

    /// Receive a round's draw. **Invoked by the MagicBlock VRF program only**,
    /// never by a user: the callback must be signed by the VRF program's scoped
    /// identity PDA for this program, which nothing else can produce.
    ///
    /// Writes the 32-byte draw onto the round it was requested for and moves it
    /// to `Drawn`, which is the only status `resolve_push` will settle under.
    /// Write-once, so a replayed or late callback - including one for a round
    /// that already expired into refunds - is refused.
    ///
    /// * `randomness` - the oracle's verified output, appended to our
    ///   discriminator by the VRF program after it checks the proof.
    pub fn consume_randomness(ctx: Context<ConsumeRandomness>, randomness: [u8; 32]) -> Result<()> {
        instructions::round::consume_randomness(ctx, randomness)
    }

    /// Void a round that has been stalled for `ROUND_EXPIRY_SLOTS`, making all
    /// of its members refundable. Permissionless. A round whose draw has landed
    /// is `Drawn` and reports no stall slot at all, so this can never be used to
    /// duck an unfavourable roll.
    pub fn expire_round(ctx: Context<ExpireRound>) -> Result<()> {
        instructions::round::expire_round(ctx)
    }

    /// Return a fully resolved round's rent to the member who opened it. Optional
    /// cleanup, permissionless, and the recipient is fixed on chain.
    pub fn close_round_account(ctx: Context<CloseRoundAccount>) -> Result<()> {
        instructions::round::close_round_account(ctx)
    }

    /// Settle or refund a push. Permissionless; the player never signs again.
    /// While the star is alive, `push_id` must equal `star.settle_cursor`.
    /// A refund does not require fulfilled randomness.
    pub fn resolve_push(ctx: Context<ResolvePush>) -> Result<()> {
        instructions::resolve_push::resolve_push(ctx)
    }

    /// Return a finished push account's rent to the player. Optional cleanup.
    pub fn close_push(ctx: Context<ClosePush>) -> Result<()> {
        instructions::resolve_push::close_push(ctx)
    }

    /// The Star Killer collects the jackpot.
    pub fn claim_prize(ctx: Context<ClaimPrize>) -> Result<()> {
        instructions::claim_prize::claim_prize(ctx)
    }

    /// An early feeder collects their share of a collapsed star, from either an
    /// Event Horizon or a stall.
    pub fn claim_hole_share(ctx: Context<ClaimHoleShare>) -> Result<()> {
        instructions::claim_hole::claim_hole_share(ctx)
    }

    /// Finish a star that stopped gaining mass for `STALL_SECS` - a day if it
    /// never left its nursery. Permissionless, and the liveness floor of the
    /// whole program: it is what guarantees no SOL can be stuck if the oracle
    /// dies or the game does.
    ///
    /// Feeders then claim the prize-side value of their own feeds through
    /// `claim_hole_share`; the rest of the pot recycles into the next star. So a
    /// stall pays nobody a profit, and the queue has to be empty first, which
    /// means it can never cancel a push that still had a roll coming.
    pub fn collapse_stalled_star(ctx: Context<CollapseStalledStar>) -> Result<()> {
        instructions::collapse_stalled::collapse_stalled_star(ctx)
    }

    /// Move accrued protocol revenue to the treasury frozen at initialize.
    /// Cannot touch escrowed pushes or unclaimed prizes.
    ///
    /// Anyone can crank it down to `DRAW_FLOAT_FLOOR`, so the house never has to
    /// be online to be paid; below that the treasury has to sign for itself,
    /// because the same balance is what buys the game its randomness.
    pub fn withdraw_protocol_fees(ctx: Context<WithdrawProtocolFees>, amount: u64) -> Result<()> {
        instructions::admin::withdraw_protocol_fees(ctx, amount)
    }

    /// Donate lamports into the protocol balance, which is what randomness is
    /// bought out of. Needed once at bootstrap, because the rake cannot accrue
    /// until a push settles and a push cannot settle until a draw is paid for.
    pub fn fund_protocol(ctx: Context<FundProtocol>, amount: u64) -> Result<()> {
        instructions::admin::fund_protocol(ctx, amount)
    }
}
