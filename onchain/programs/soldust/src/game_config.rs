//! # Central game configuration
//!
//! These numbers **are** the game. They are compiled in. There is no
//! instruction that can retune them, and mainnet upgrade authority is `None`,
//! so the deployed binary is the whole rulebook.
//!
//! Nothing reads these from an account. Handlers call [`economics`] /
//! [`lifecycle`]. A live [`Star`](crate::state::Star) copies the lifecycle at
//! birth, which now only protects its STARDUST table - the odds are not a
//! table at all.
//!
//! ## The odds
//!
//! A push's chance of going nova is its own share of the mass it creates:
//!
//! ```text
//! p = amount / (mass_before + amount)
//! ```
//!
//! Your stake against the pot you are shooting at, as straight odds. There is
//! no per-stage rate, no exposure unit, no compounding and no cap. See
//! [`nova_ppb`] for why this is the only honest curve:
//!
//! * **It pays exactly `prize_bps`.** The killer takes `prize_pool` measured
//!   after their own push lands, and `prize_pool` is exactly `prize_bps` of
//!   mass, so `EV = [a / (M+a)] * 0.9686(M+a) = 0.9686a`. The `(M+a)` cancels:
//!   every bid size, at every pot size, at every stage, returns 96.86%. The
//!   3.14% rake is the only edge, and it is taken at claim time.
//! * **Survival telescopes.** One push survives with probability `M/(M+a)`, so
//!   a run of pushes survives with `M0/M1 * M1/M2 * ... = M0/Mn`. Star mass is
//!   a martingale. A star therefore reaches the hole exactly
//!   `FEED_MASS / HOLE_MASS` = 1/21 of the time, whatever sizes people pushed,
//!   and nursery feeders are taking fair 21:1 odds on that - which returns
//!   them the same 96.86%. Feeders and pushers sit on one book by construction
//!   rather than by tuning.
//! * **The queue cannot hurt you.** The threshold is computed at settle time
//!   from settled mass. If someone lands ahead of you your chance falls and
//!   the pot you are shooting at rises by the same factor, so your EV does not
//!   move. Nothing a signed amount depends on can go stale, which is why there
//!   is no longer any minimum to miss.
//!
//! The only shape rule left is [`PUSH_STEP`]: whole multiples of 0.01 SOL,
//! clipped to the room left to the nursery or hole boundary.

use anchor_lang::prelude::*;

use crate::constants::{BPS, FEED_MASS, HOLE_MASS, LAMPORTS_PER_SOL, MAX_STAGES, PPB, PUSH_STEP};

const fn sol(whole: u64, milli: u64) -> u64 {
    whole * LAMPORTS_PER_SOL + milli * (LAMPORTS_PER_SOL / 1_000)
}

/// One lifecycle stage: the mass at which it begins and how much STARDUST it
/// mints.
///
/// Deliberately carries no odds. Stages are cosmetic and economic flavour now;
/// lethality is [`nova_ppb`], which is structural.
#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageConfig {
    /// Inclusive lower bound, in lamports of star mass, for this stage.
    pub min_mass: u64,
    /// STARDUST multiplier in bps (10000 = 1×). Early stages mint more.
    pub stardust_mult_bps: u16,
}

/// The stage table, frozen onto each star at birth.
#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleConfig {
    pub stage_count: u8,
    pub stages: [StageConfig; MAX_STAGES],
}

/// Money. `prize_bps + protocol_bps` must equal 10000.
///
/// The house edge is 314 bps - pi, to two places, and a hair under the 350 bps
/// the nearest comparable game charges. It is not an arbitrary round number:
/// `protocol_bps` of one [`PUSH_STEP`] has to be a whole number of lamports or
/// [`Economics::split`] would round and `prize_pool` would stop being exactly
/// `prize_bps` of mass, which is what makes the return identity exact. 314 bps
/// of 0.01 SOL is 314 000 lamports on the nose.
#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Economics {
    pub prize_bps: u16,
    pub protocol_bps: u16,

    pub stardust_per_sol: u64,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        // Stage boundaries drive the visuals and the STARDUST taper. Early
        // mass mints 2.5× because it is the money most at risk of simply
        // never being hit: it is also the money that owns the hole.
        //
        // Nursery (feeds to 1 SOL) sits inside PROTOSTAR, which holds through
        // 2 SOL so the first last-hits are still that look. Then 2 → 21 is four
        // even 4 SOL bands and a 3 SOL Critical; the hole stays 21.
        Self {
            stage_count: 7,
            stages: [
                StageConfig { min_mass: sol(0, 0), stardust_mult_bps: 25_000 },
                StageConfig { min_mass: sol(2, 0), stardust_mult_bps: 15_000 },
                StageConfig { min_mass: sol(6, 0), stardust_mult_bps: 13_000 },
                StageConfig { min_mass: sol(10, 0), stardust_mult_bps: 11_500 },
                StageConfig { min_mass: sol(14, 0), stardust_mult_bps: 10_000 },
                StageConfig { min_mass: sol(18, 0), stardust_mult_bps: 9_000 },
                StageConfig { min_mass: sol(21, 0), stardust_mult_bps: 8_000 },
            ],
        }
    }
}

impl Default for Economics {
    fn default() -> Self {
        Self {
            prize_bps: 9_686,
            protocol_bps: 314,
            stardust_per_sol: 1_000,
        }
    }
}

/// Locked economics. Handlers use this; it is never read from an account.
pub fn economics() -> Economics {
    Economics::default()
}

/// Locked lifecycle. New stars copy this at birth.
pub fn lifecycle() -> LifecycleConfig {
    LifecycleConfig::default()
}

// ------------------------------------------------------------------- the odds

/// The whole odds model: a push's share of the mass it creates, in ppb.
///
/// `mass_after` is `mass_before + accepted`, i.e. the mass the star has once
/// this push has landed - the same mass whose `prize_bps` share the winner
/// collects. Pass the two together and the fairness is arithmetic rather than
/// a tuned constant.
///
/// Truncates, so a push's realised chance is never above the exact ratio and
/// never pays over fair. The gap is at most one part per billion, and the
/// crumb accrues to the pot rather than to anyone in particular.
pub fn nova_ppb(accepted: u64, mass_after: u64) -> u32 {
    if accepted == 0 || mass_after == 0 {
        return 0;
    }
    let p = (accepted as u128).saturating_mul(PPB as u128) / mass_after as u128;
    core::cmp::min(p, PPB as u128) as u32
}

/// Round `v` down onto the [`PUSH_STEP`] lattice.
///
/// Mass on a star this build created is always a whole number of steps, so
/// this is a no-op in practice. It matters for a star carrying an endowment or
/// legacy mass that predates the step rule: the boundary room must still be a
/// legal push or the last fill could never land.
pub fn floor_to_step(v: u64) -> u64 {
    v - (v % PUSH_STEP)
}

impl LifecycleConfig {
    pub fn stage_for_mass(&self, mass: u64) -> u8 {
        let n = self.stage_count as usize;
        let mut idx = 0usize;
        for i in 0..n {
            if mass >= self.stages[i].min_mass {
                idx = i;
            } else {
                break;
            }
        }
        idx as u8
    }

    /// Stage table multiplier. Nursery is 2.5×; later stages taper.
    pub fn stardust_mult_bps_at(&self, mass: u64) -> u16 {
        let i = self.stage_for_mass(mass) as usize;
        self.stages[i].stardust_mult_bps
    }
}

impl Economics {
    /// Lamports left before `mass` hits the next boundary: the nursery cap
    /// below 1 SOL, the hole cap above it. Always a whole number of steps, so
    /// the value is itself a legal push.
    pub fn room_at(mass: u64) -> u64 {
        let raw = if mass < FEED_MASS {
            FEED_MASS - mass
        } else {
            HOLE_MASS.saturating_sub(mass)
        };
        floor_to_step(raw)
    }

    /// The only two things that can reject a push: zero, and off-lattice.
    ///
    /// Oversize clips to `room` instead of failing, so a push cannot be
    /// invalidated by anything that happens between signing and landing. That
    /// plus the absence of a minimum is what makes a signed amount safe.
    pub fn accept_push_amount(amount: u64, room: u64) -> Result<u64> {
        require!(amount > 0, crate::errors::SoldustError::PushAmountOutOfRange);
        require!(
            amount % PUSH_STEP == 0,
            crate::errors::SoldustError::PushNotOnStep
        );
        let room = floor_to_step(room);
        // A live star always has room: `feed` requires mass below the nursery
        // cap and `request_push` requires committed mass below the hole cap,
        // and both boundaries are whole steps. If room were somehow zero we
        // would rather overshoot the hole (which just collapses the star into
        // a bigger pot) than land nothing - `resolve_push` has no zero-fill
        // branch. See `settle_always_lands_something`.
        if room == 0 {
            return Ok(amount);
        }
        Ok(amount.min(room))
    }

    /// How much of a pending last-hit actually lands. The queue quoted room
    /// against committed mass; by the time this push reaches the head, settled
    /// mass may leave less. Always at least one whole step.
    pub fn accepted_settle_amount(&self, amount: u64, star_mass: u64) -> u64 {
        let room = Self::room_at(star_mass);
        if room == 0 {
            return amount;
        }
        amount.min(room)
    }

    /// (prize, protocol). Exact for any multiple of [`PUSH_STEP`], because
    /// `protocol_bps` of one step is a whole 314_000 lamports. Prize absorbs
    /// any remainder anyway, so the two always sum back to `amount`.
    pub fn split(&self, amount: u64) -> Result<(u64, u64)> {
        let protocol = mul_bps(amount, self.protocol_bps)?;
        let prize = amount
            .checked_sub(protocol)
            .ok_or(crate::errors::SoldustError::MathOverflow)?;
        Ok((prize, protocol))
    }

    pub fn stardust_for(
        &self,
        amount: u64,
        stage_mult_bps: u16,
        chance_ppb: u32,
    ) -> Result<u64> {
        let variable = (amount as u128)
            .checked_mul(self.stardust_per_sol as u128)
            .ok_or(crate::errors::SoldustError::MathOverflow)?
            / (LAMPORTS_PER_SOL as u128);
        let staged = variable
            .checked_mul(stage_mult_bps as u128)
            .ok_or(crate::errors::SoldustError::MathOverflow)?
            / BPS as u128;
        let awarded = staged
            .checked_mul(chance_stardust_bps(chance_ppb) as u128)
            .ok_or(crate::errors::SoldustError::MathOverflow)?
            / BPS as u128;
        u64::try_from(awarded).map_err(|_| crate::errors::SoldustError::MathOverflow.into())
    }
}

fn mul_bps(amount: u64, bps: u16) -> Result<u64> {
    let v = (amount as u128)
        .checked_mul(bps as u128)
        .ok_or(crate::errors::SoldustError::MathOverflow)?
        / BPS as u128;
    u64::try_from(v).map_err(|_| crate::errors::SoldustError::MathOverflow.into())
}

/// STARDUST sweetener for buying a bigger slice of the star, piecewise linear
/// on the push's own nova chance: 1% → 10000, 2% → 10300, 5% → 11000,
/// 10% → 12000, flat above that.
///
/// Keyed on chance rather than on a multiple of some minimum, because there is
/// no minimum any more. The anchors are the same reference points the old
/// exposure curve used - a 1× push was 1% of the pot, which is ~1% chance - so
/// mint rates land where they always did.
pub fn chance_stardust_bps(chance_ppb: u32) -> u32 {
    let x = chance_ppb as u64;
    if x <= 10_000_000 {
        return 10_000;
    }
    if x <= 20_000_000 {
        return (10_000 + 300 * (x - 10_000_000) / 10_000_000) as u32;
    }
    if x <= 50_000_000 {
        return (10_300 + 700 * (x - 20_000_000) / 30_000_000) as u32;
    }
    if x <= 100_000_000 {
        return (11_000 + 1_000 * (x - 50_000_000) / 50_000_000) as u32;
    }
    12_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc() -> LifecycleConfig {
        LifecycleConfig::default()
    }

    /// Chance as a float, for the probability identities below.
    fn p(amount: u64, mass_before: u64) -> f64 {
        nova_ppb(amount, mass_before + amount) as f64 / PPB as f64
    }

    /// What the winner collects if this push kills: `prize_bps` of the mass
    /// that exists once it has landed.
    fn payout(mass_after: u64) -> u64 {
        Economics::default().split(mass_after).unwrap().0
    }

    /// The invariants that used to be a runtime `validate()`. They are checked
    /// here instead, because the values are compile-time constants: if these
    /// hold at build time they cannot be violated on chain.
    #[test]
    fn shipped_table_is_well_formed() {
        let c = lc();
        let n = c.stage_count as usize;
        assert!((1..=MAX_STAGES).contains(&n));
        assert_eq!(c.stages[0].min_mass, 0);
        for i in 1..n {
            assert!(c.stages[i].min_mass > c.stages[i - 1].min_mass);
        }

        let e = Economics::default();
        assert_eq!(e.prize_bps as u64 + e.protocol_bps as u64, BPS);

        // Both boundaries must sit on the lattice or the room left to them
        // would not be a legal push.
        assert_eq!(FEED_MASS % PUSH_STEP, 0);
        assert_eq!(HOLE_MASS % PUSH_STEP, 0);
        assert_eq!(PUSH_STEP, sol(0, 10));
    }

    #[test]
    fn stage_boundaries() {
        let c = lc();
        assert_eq!(c.stage_for_mass(0), 0);
        assert_eq!(c.stage_for_mass(sol(1, 0)), 0);
        assert_eq!(c.stage_for_mass(sol(1, 999)), 0);
        assert_eq!(c.stage_for_mass(sol(2, 0)), 1);
        assert_eq!(c.stage_for_mass(sol(5, 999)), 1);
        assert_eq!(c.stage_for_mass(sol(6, 0)), 2);
        assert_eq!(c.stage_for_mass(sol(18, 0)), 5);
        assert_eq!(c.stage_for_mass(sol(20, 999)), 5);
        assert_eq!(c.stage_for_mass(sol(21, 0)), 6);
    }

    // --------------------------------------------------------------- the odds

    /// The numbers the model was specified against.
    #[test]
    fn quoted_examples() {
        // 0.01 SOL into a 1 SOL star: 0.01/1.01.
        assert_eq!(nova_ppb(sol(0, 10), sol(1, 10)), 9_900_990);
        // 20 SOL into a 1 SOL star: 20/21. One shot, and it is fair.
        assert_eq!(nova_ppb(sol(20, 0), sol(21, 0)), 952_380_952);
        // 1 SOL of a 21 SOL star: the feeder's 21:1.
        assert_eq!(nova_ppb(sol(1, 0), sol(21, 0)), 47_619_047);
        // The smallest legal push against the largest legal pot still has
        // meaningful resolution: 0.01/21.
        assert_eq!(nova_ppb(sol(0, 10), sol(21, 0)), 476_190);
    }

    /// The identity the whole design rests on: `EV = prize_bps * amount`, for
    /// any amount, at any mass. Truncation in `nova_ppb` is the only error and
    /// it always favours the house.
    #[test]
    fn return_is_exactly_prize_bps() {
        let mut checked = 0usize;
        for &mass in &[
            FEED_MASS,
            sol(1, 10),
            sol(1, 400),
            sol(3, 200),
            sol(5, 400),
            sol(9, 200),
            sol(14, 500),
            sol(20, 990),
        ] {
            let room = Economics::room_at(mass);
            for &amount in &[
                PUSH_STEP,
                sol(0, 20),
                sol(0, 50),
                sol(0, 100),
                sol(1, 0),
                sol(5, 0),
                room,
            ] {
                if amount == 0 || amount > room {
                    continue;
                }
                let mass_after = mass + amount;
                let ppb = nova_ppb(amount, mass_after) as u128;
                let ev = ppb * payout(mass_after) as u128 / PPB as u128;
                let want = payout(amount) as u128;

                // Never pays over fair.
                assert!(ev <= want, "mass {mass} amount {amount}: {ev} > {want}");
                // And never by more than the one-ppb truncation.
                let slack = payout(mass_after) as u128 / PPB as u128 + 1;
                assert!(
                    want - ev <= slack,
                    "mass {mass} amount {amount}: {ev} vs {want}, slack {slack}"
                );
                checked += 1;
            }
        }
        assert!(checked > 30, "sweep collapsed to {checked} cases");
    }

    /// `split` has no remainder for a legal push, so `prize_pool` is exactly
    /// `prize_bps` of mass and the identity above has nothing to round.
    ///
    /// This is the constraint that picks the rake out of the reals: it has to
    /// be a bps figure whose share of one [`PUSH_STEP`] is a whole number of
    /// lamports. 314 clears it exactly, which is the only reason pi survived
    /// contact with the lattice.
    #[test]
    fn split_is_exact_on_the_lattice() {
        let e = Economics::default();
        assert_eq!(e.protocol_bps, 314);
        assert_eq!(PUSH_STEP * e.protocol_bps as u64 % BPS, 0);
        for steps in [1u64, 2, 7, 100, 500, 2100] {
            let amount = steps * PUSH_STEP;
            let (prize, protocol) = e.split(amount).unwrap();
            assert_eq!(prize + protocol, amount);
            assert_eq!(protocol, steps * 314_000);
            assert_eq!(prize * BPS, amount * e.prize_bps as u64);
        }
    }

    /// Survival over a run of pushes collapses to `M0/Mn` regardless of how
    /// the run was sized. This is what makes the hole rate structural.
    ///
    /// Exact in the reals. On chain each factor carries up to a ppb of
    /// truncation, all of it in the same direction, so a long run drifts a
    /// little high - a thousand pushes is still within a part in a million.
    #[test]
    fn survival_telescopes() {
        for sizes in [
            vec![PUSH_STEP; 40],
            vec![sol(0, 50); 20],
            vec![sol(1, 0), sol(0, 10), sol(3, 0), sol(0, 20), sol(5, 0)],
            vec![sol(0, 10), sol(0, 20), sol(0, 30), sol(0, 40), sol(0, 50)],
        ] {
            let n = sizes.len() as f64;
            let m0 = FEED_MASS;
            let mut mass = m0;
            let mut survive = 1.0f64;
            for a in sizes {
                survive *= 1.0 - p(a, mass);
                mass += a;
            }
            let want = m0 as f64 / mass as f64;
            assert!(survive >= want, "survive {survive} want {want}");
            assert!(
                (survive - want) / want < n * 1e-9,
                "survive {survive} want {want}"
            );
        }
    }

    /// A star reaches Event Horizon essentially exactly `FEED_MASS/HOLE_MASS`
    /// of the time. Nobody tuned 1/21; it is the two boundaries and nothing
    /// else. Even 2000 minimum pushes leave it inside a part in a million.
    #[test]
    fn hole_rate_is_feed_over_hole() {
        let want = FEED_MASS as f64 / HOLE_MASS as f64;
        for step in [PUSH_STEP, sol(0, 50), sol(1, 0), sol(4, 0)] {
            let mut mass = FEED_MASS;
            let mut survive = 1.0f64;
            while mass < HOLE_MASS {
                let a = step.min(Economics::room_at(mass));
                survive *= 1.0 - p(a, mass);
                mass += a;
            }
            assert_eq!(mass, HOLE_MASS);
            assert!(survive >= want, "step {step}: {survive} vs {want}");
            assert!(
                (survive - want) / want < 1e-6,
                "step {step}: {survive} vs {want}"
            );
        }
        assert!((want - 1.0 / 21.0).abs() < 1e-12);
    }

    /// Feeders and pushers are on one book. A 1 SOL nursery one-shot by a
    /// 20 SOL push divides the pool in expectation into exactly `prize_bps` of
    /// each side's stake, and the two shares sum back to the pool.
    #[test]
    fn feeder_and_pusher_books_match() {
        let feed = FEED_MASS;
        let shot = sol(20, 0);
        let pool = payout(feed + shot) as f64;

        let pusher = p(shot, feed) * pool;
        let feeder = (1.0 - p(shot, feed)) * pool;

        assert!((pusher - payout(shot) as f64).abs() < 32.0, "pusher {pusher}");
        assert!((feeder - payout(feed) as f64).abs() < 32.0, "feeder {feeder}");
        assert!((pusher + feeder - pool).abs() < 1.0);
    }

    /// The same thing over a whole star's life: whatever the path, a feeder's
    /// stake returns `prize_bps` through the hole alone.
    #[test]
    fn feeders_return_prize_bps_through_the_hole() {
        let stake = sol(0, 250); // a quarter of the nursery
        let hole_odds = FEED_MASS as f64 / HOLE_MASS as f64;
        let share = stake as f64 / FEED_MASS as f64;
        let ev = hole_odds * share * payout(HOLE_MASS) as f64;
        assert!(
            (ev - payout(stake) as f64).abs() < 1.0,
            "{ev} vs {}",
            payout(stake)
        );
    }

    /// There is no cap, so the only ceiling on a single push is the room left.
    /// One-shots are cheap on a young star and impossible near the hole.
    #[test]
    fn max_chance_is_room_to_the_hole() {
        for (mass, want) in [
            (FEED_MASS, 0.952),
            (sol(5, 0), 0.762),
            (sol(10, 0), 0.524),
            (sol(14, 500), 0.310),
            (sol(20, 0), 0.048),
        ] {
            let room = Economics::room_at(mass);
            let got = p(room, mass);
            assert!((got - want).abs() < 0.001, "mass {mass}: {got} vs {want}");
            // Which is just `1 - mass/HOLE_MASS`.
            let closed = 1.0 - mass as f64 / HOLE_MASS as f64;
            assert!((got - closed).abs() < 1e-9);
        }
    }

    // ------------------------------------------------------------- step rule

    #[test]
    fn only_off_lattice_and_zero_are_rejected() {
        let room = sol(1, 0);
        assert_eq!(Economics::accept_push_amount(PUSH_STEP, room).unwrap(), PUSH_STEP);
        assert_eq!(
            Economics::accept_push_amount(sol(0, 120), room).unwrap(),
            sol(0, 120)
        );
        // Oversize clips instead of failing.
        assert_eq!(Economics::accept_push_amount(sol(5, 0), room).unwrap(), room);
        // 0.012 SOL is not a whole step.
        assert!(Economics::accept_push_amount(sol(0, 12), room).is_err());
        assert!(Economics::accept_push_amount(PUSH_STEP - 1, room).is_err());
        assert!(Economics::accept_push_amount(0, room).is_err());
    }

    #[test]
    fn room_stays_on_the_lattice() {
        assert_eq!(Economics::room_at(0), FEED_MASS);
        assert_eq!(Economics::room_at(sol(0, 500)), sol(0, 500));
        assert_eq!(Economics::room_at(FEED_MASS), HOLE_MASS - FEED_MASS);
        assert_eq!(Economics::room_at(sol(20, 990)), sol(0, 10));
        assert_eq!(Economics::room_at(HOLE_MASS), 0);
        assert_eq!(Economics::room_at(sol(60, 0)), 0);
        // Off-lattice mass rounds down to the last legal push, and a star
        // inside one step of a boundary has no legal room at all. Endowments
        // are floored to the lattice at birth so this only describes mass
        // that predates the step rule.
        assert_eq!(Economics::room_at(sol(20, 983)), sol(0, 10));
        assert_eq!(Economics::room_at(sol(20, 993)), 0);
        for mass in [0, 1, PUSH_STEP - 1, FEED_MASS - 1, sol(7, 777) + 13] {
            assert_eq!(Economics::room_at(mass) % PUSH_STEP, 0);
        }
    }

    /// A push that landed on the lattice keeps mass on the lattice, so the
    /// last fill to either boundary is always exactly legal.
    #[test]
    fn lattice_is_closed_under_pushes() {
        let mut mass = 0u64;
        while mass < HOLE_MASS {
            let room = Economics::room_at(mass);
            let want = sol(0, 70).min(room);
            let got = Economics::accept_push_amount(sol(0, 70), room).unwrap();
            assert_eq!(got, want);
            mass += got;
            assert_eq!(mass % PUSH_STEP, 0);
        }
        assert_eq!(mass, HOLE_MASS);
    }

    #[test]
    fn settle_clips_to_hole_room() {
        let e = Economics::default();
        assert_eq!(e.accepted_settle_amount(sol(3, 0), sol(20, 800)), sol(0, 200));
        assert_eq!(e.accepted_settle_amount(sol(0, 50), sol(5, 0)), sol(0, 50));
        // Nursery clips to the nursery cap, not the hole cap.
        assert_eq!(e.accepted_settle_amount(sol(5, 0), sol(0, 400)), sol(0, 600));
    }

    /// `resolve_push` refunds *part* of a push when the room computed on
    /// settled mass is below what the queue-aware room quoted at request time,
    /// but it has no branch for a push that lands nothing, and
    /// `request_push_vrf` buys an oracle unconditionally for the live head.
    /// Both rely on this: a push that passed `accept_push_amount` always lands
    /// at least one whole step.
    #[test]
    fn settle_always_lands_something() {
        let e = Economics::default();
        let mut checked = 0usize;
        for &mass in &[
            0,
            sol(0, 500),
            FEED_MASS,
            sol(1, 400),
            sol(5, 400),
            sol(14, 500),
            sol(20, 990),
            sol(21, 0),
            sol(60, 0),
        ] {
            for &pending in &[0, PUSH_STEP, sol(5, 0), sol(500, 0)] {
                let room = Economics::room_at(mass.saturating_add(pending));
                for &want in &[PUSH_STEP, sol(1, 0), sol(500, 0), room] {
                    let Ok(amount) = Economics::accept_push_amount(want, room) else {
                        continue;
                    };
                    let landed = e.accepted_settle_amount(amount, mass);
                    assert!(
                        landed >= PUSH_STEP,
                        "mass {mass} pending {pending} amount {amount} landed {landed}"
                    );
                    assert_eq!(landed % PUSH_STEP, 0);
                    checked += 1;
                }
            }
        }
        assert!(checked > 50, "sweep collapsed to {checked} cases");
    }

    // -------------------------------------------------------------- stardust

    #[test]
    fn stardust_scales_with_sol_and_stage() {
        let e = Economics::default();
        let one_pct = nova_ppb(sol(0, 10), sol(1, 10));
        assert_eq!(e.stardust_for(sol(1, 0), 25_000, one_pct).unwrap(), 2_500);
        assert_eq!(e.stardust_for(sol(1, 0), 0, one_pct).unwrap(), 0);
        // A push big enough to be a coin flip mints the full 1.2× sweetener.
        assert_eq!(
            e.stardust_for(sol(1, 0), 25_000, nova_ppb(sol(1, 0), sol(2, 0))).unwrap(),
            3_000
        );
    }

    #[test]
    fn chance_sweetener_matches_the_old_anchors() {
        // The old curve's reference points, restated as chances.
        assert_eq!(chance_stardust_bps(nova_ppb(sol(0, 10), sol(1, 10))), 10_000);
        assert_eq!(chance_stardust_bps(10_000_000), 10_000);
        assert_eq!(chance_stardust_bps(20_000_000), 10_300);
        assert_eq!(chance_stardust_bps(50_000_000), 11_000);
        assert_eq!(chance_stardust_bps(100_000_000), 12_000);
        assert_eq!(chance_stardust_bps(PPB as u32), 12_000);
        // Monotonic, no steps backwards.
        let mut last = 0;
        for x in (0..=100_000_000u32).step_by(1_000_000) {
            let got = chance_stardust_bps(x);
            assert!(got >= last, "{x}: {got} < {last}");
            last = got;
        }
    }

    #[test]
    fn stardust_declines_across_stages() {
        let l = LifecycleConfig::default();
        assert_eq!(l.stardust_mult_bps_at(0), 25_000);
        assert_eq!(l.stardust_mult_bps_at(sol(1, 999)), 25_000);
        assert_eq!(l.stardust_mult_bps_at(sol(2, 0)), 15_000);
        assert_eq!(l.stardust_mult_bps_at(sol(14, 0)), 10_000);
        assert_eq!(l.stardust_mult_bps_at(sol(21, 0)), 8_000);
    }
}
