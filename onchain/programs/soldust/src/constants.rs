//! PDA seeds and fixed-point denominators.

/// Singleton game configuration.
pub const CONFIG_SEED: &[u8] = b"config";
/// Single SOL vault holding escrow + prizes + protocol fees.
pub const VAULT_SEED: &[u8] = b"vault";
/// One per star: `[STAR_SEED, star_id.to_le_bytes()]`.
pub const STAR_SEED: &[u8] = b"star";
/// One per push: `[PUSH_SEED, player, client_seed]`.
pub const PUSH_SEED: &[u8] = b"push";
/// One per VRF draw: `[ROUND_SEED, star_id_le, round_id_le]`.
pub const ROUND_SEED: &[u8] = b"round";
/// One per wallet: `[PLAYER_SEED, wallet]`.
pub const PLAYER_SEED: &[u8] = b"player";
/// Per-star early-feed tally: `[FEED_SEED, star_id_le]`.
pub const FEED_SEED: &[u8] = b"feed";
/// Per-player share of a star's hole: `[FEED_SHARE_SEED, star_id_le, wallet]`.
pub const FEED_SHARE_SEED: &[u8] = b"feed-share";

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Pushes below this mass buy a hole ticket. No supernova.
pub const FEED_MASS: u64 = LAMPORTS_PER_SOL;
/// Survive to this mass without a nova and the star collapses. Event Horizon.
pub const HOLE_MASS: u64 = 21 * LAMPORTS_PER_SOL;

/// The absolute, permanent granularity *and* floor of every push: 0.01 SOL.
///
/// A push is a whole number of these or it is rejected. That is the only shape
/// rule in the game, and it buys three things:
///
/// * 0.01 SOL is legal forever, on any star, at any jackpot. Nothing the chain
///   does between signing and landing can make a signed amount invalid.
/// * Mass stays on a 0.01 SOL lattice - [`FEED_MASS`] is 100 steps and
///   [`HOLE_MASS`] is 2100 - so the room left to either boundary is always
///   itself a legal push and the last fill never has to be a special case.
/// * `protocol_bps` of a whole step is an exact integer (250_000 lamports), so
///   [`Economics::split`](crate::game_config::Economics::split) never rounds
///   and `prize_pool` is exactly `prize_bps` of mass.
pub const PUSH_STEP: u64 = LAMPORTS_PER_SOL / 100;

/// Probabilities are parts-per-billion. A push's nova chance is its share of
/// the mass it creates, and the smallest legal push against the largest legal
/// pot is 0.01/21 - roughly 476_190 ppb. Billionths keep even that exact to
/// six figures, and 1e9 still fits a `u32`.
pub const PPB: u64 = 1_000_000_000;

/// Revenue splits are basis points.
pub const BPS: u64 = 10_000;

/// Lifecycle stages in the shipped curve.
pub const MAX_STAGES: usize = 7;

/// The draw float a *permissionless* fee withdrawal has to leave behind.
///
/// `withdraw_protocol_fees` is open to anyone, so the house never has to be
/// online to be paid - but the balance it moves is also the float `draw_round`
/// reimburses cranks from. Left completely open, a stranger could sweep the rake
/// to the treasury the moment it accrued, and at zero every draw comes out of a
/// cranker's own pocket: the house crank keeps working and eats the cost, and no
/// third party has any reason to run one at all.
///
/// So a permissionless withdrawal may only take what is above this, and the
/// treasury can sign for its own withdrawal to take the rest. 0.05 SOL is about
/// twenty mainnet draws - enough to keep a star moving until a human notices,
/// small enough that it is never where the revenue is.
pub const DRAW_FLOAT_FLOOR: u64 = LAMPORTS_PER_SOL / 20;

// ------------------------------------------------------------------- rounds
//
// One ORAO draw serves every push queued in the same round, so the oracle
// costs `cost / member_count` per push instead of `cost` per push. Slots are
// roughly 400ms; the figures below are quoted in slots because that is what
// the runtime gives us and it cannot be gamed by a validator clock.

/// How long a round stays open before anyone may close it. ~30 seconds.
///
/// This is the whole latency/cost dial. Longer batches more pushes into one
/// draw and makes each one cheaper; shorter settles sooner. Thirty seconds is
/// long enough that a handful of players are not racing the clock, and short
/// enough that a full room still feels like one beat. It is measured from the
/// *first* push in the round, so a quiet star does not make anyone wait for a
/// window that started before they arrived.
///
/// Note that a deep queue is strictly *faster* under rounds than it was under
/// one-request-per-push: every member of a round settles after a single
/// fulfilment, where before each push had to wait for the one ahead of it to
/// buy and receive its own randomness.
pub const ROUND_WINDOW_SLOTS: u64 = 75;

/// A round this full may be closed immediately, without waiting out the
/// window. Purely an optimisation: under load the draw is already well
/// amortised, so there is no reason to keep people waiting.
pub const ROUND_TARGET_MEMBERS: u64 = 24;

/// How long a round may sit un-drawn before anyone can void it. ~5 minutes.
///
/// This is the escape hatch that stops a star freezing forever. If ORAO stops
/// answering, or the crank dies between closing a round and buying its draw,
/// every push in that round becomes refundable and the star carries on with a
/// fresh round. Without it an unfulfilled request would strand the queue head
/// permanently, and with no upgrade authority there would be no way back.
///
/// Generous on purpose: ORAO normally answers inside a second, so five minutes
/// only fires when something is genuinely broken. A round that expires costs
/// the house the draw it already paid for and costs players nothing but time.
pub const ROUND_EXPIRY_SLOTS: u64 = 750;

// ------------------------------------------------------------------- stalling
//
// `ROUND_EXPIRY_SLOTS` gets every *stake* out of a broken round, and that used
// to be the whole story. It is not enough on its own: refunds do not add mass,
// so a star whose pushes only ever refund stays Alive below the hole cap
// forever, and the pot already paid in by earlier settles has no claimant -
// no kill to pay a winner, no Event Horizon to pay the feeders. Play cannot
// advance past it either, because a successor needs the incumbent closed.
//
// So a star that stops moving can be collapsed by anyone. Feeders take back
// what their own feeds put into the pot and the rest recycles into the next
// star, which is the one payout policy that leaves nobody with a reason to
// want a stall: it never pays more than cost, so a quiet spell cannot be
// farmed, and no lamport leaves the game.
//
// Measured in seconds off the cluster clock rather than in slots. These are
// day-scale windows and slot time drifts by tens of percent under load - a
// week counted in slots can land a day out.

/// Silence that finishes a star nobody ever really played.
///
/// Mass still at nursery level means nothing is at stake beyond the feeds
/// themselves: there is no jackpot being protected, so there is no reason to
/// make feeders wait a week for money that was never going anywhere.
#[cfg(not(feature = "short-stalls"))]
pub const NURSERY_STALL_SECS: i64 = 24 * 60 * 60;

/// Silence that finishes a star that was genuinely under way.
///
/// A week, not a day, and the reason is the feeders' own expected value rather
/// than the house's. Their ticket pays a share of the whole pot if the star
/// reaches Event Horizon; collapsing early swaps that for a refund at cost. A
/// short fuse would cash out every feeder's upside the first quiet day, which
/// on a young game is most days. A week only fires when the game is actually
/// dead - the one case where the ticket was worth nothing anyway.
#[cfg(not(feature = "short-stalls"))]
pub const STALL_SECS: i64 = 7 * 24 * 60 * 60;

/// Localnet only, and never what deploys.
///
/// A day-scale timeout cannot be waited out in a test, so
/// `localnet/scenarios/c3-stalled-star.ts` runs against a build with this
/// feature on - see `localnet/build-short-stalls.sh` and the `SOLDUST_SO` hook
/// in `localnet/validator.sh`. The shipped values above are asserted in
/// `state.rs` under `#[cfg(not(feature = "short-stalls"))]`, so a build that
/// carries these numbers cannot also pass the test suite.
#[cfg(feature = "short-stalls")]
pub const NURSERY_STALL_SECS: i64 = 10;
#[cfg(feature = "short-stalls")]
pub const STALL_SECS: i64 = 25;
