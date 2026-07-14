//! decay — confidence that fades without reuse, in pure integer math.
//!
//! A memory carries a `confidence` in micro-units (0..=1_000_000, where
//! 1_000_000 == full) and a `last_used` instant. Without reuse the confidence
//! decays on a half-life: after one half-life it is halved, after two a
//! quarter, and so on. Reuse (recall `--touch`, or an explicit mark) resets
//! `last_used` and restores full confidence.
//!
//! # Why integer-only
//!
//! Recall blends decay into its ranking, and the gate bans floats in the
//! scoring paths (`src/recall`, `src/util`). This module is therefore i64/i128
//! only: whole half-lives halve by shifting; the sub-half-life remainder is a
//! linear interpolation between 1 and 1/2, which is monotone, deterministic,
//! and never diverges across platforms. The approximation is deliberately mild
//! (decay is a small prior, not a dominant term) so precision beyond this is
//! wasted.

/// Full confidence in micro-units (1.0). A memory with no `confidence` field
/// is treated as this: absent means pristine, not zero.
pub const FULL_CONFIDENCE_MICROS: i64 = 1_000_000;

/// Default half-life for confidence decay: 90 days. A memory untouched for 90
/// days sits at half confidence, at 180 days a quarter, and so on.
pub const DEFAULT_HALF_LIFE_SECS: i64 = 60 * 60 * 24 * 90;

/// Default blend weight of the decay prior in recall (micro-units). The prior
/// contributes at most this much of a *demotion* to a fully-decayed memory's
/// score; a fresh memory is untouched. Kept small so BM25 / embedding ranking
/// still dominates — decay only reorders near-ties and sinks stale cruft.
pub const DEFAULT_DECAY_WEIGHT_MICROS: i64 = 200_000;

/// Default floor for `prune`: a decayed confidence below this (0.25) is stale
/// enough to archive. Overridable with `--below <micros>`.
pub const DEFAULT_PRUNE_FLOOR_MICROS: i64 = 250_000;

/// Decayed confidence in micro-units, given the stored `base` confidence
/// (micros), the reference instant it was last fresh (`last_used`, epoch
/// seconds), the query-time `now` (epoch seconds), and the `half_life` in
/// seconds.
///
/// Purely integer. `elapsed = max(0, now - last_used)`; whole half-lives halve
/// the value by shifting; the remaining fraction linearly interpolates the
/// last halving. `now <= last_used` (or a non-positive half-life) returns the
/// base unchanged. Result is clamped to `0..=base`.
pub fn decayed_confidence_micros(base: i64, last_used: i64, now: i64, half_life: i64) -> i64 {
    let base = base.clamp(0, FULL_CONFIDENCE_MICROS);
    if half_life <= 0 {
        return base;
    }
    let elapsed = now.saturating_sub(last_used);
    if elapsed <= 0 {
        return base;
    }
    let whole = elapsed / half_life;
    // More than 63 half-lives: the memory has decayed to nothing.
    if whole >= 63 {
        return 0;
    }
    // Halve once per whole half-life (i128 to keep the math obvious/safe).
    let mut v = i128::from(base) >> whole;
    // Fractional remainder: linear interpolation from 1 down to 1/2 across the
    // partial half-life, i.e. multiply by (2*half_life - frac) / (2*half_life).
    let frac = elapsed % half_life;
    if frac > 0 {
        let num = i128::from(2 * half_life - frac);
        let den = i128::from(2 * half_life);
        v = (v * num) / den;
    }
    v.clamp(0, i128::from(base)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const HL: i64 = DEFAULT_HALF_LIFE_SECS;

    #[test]
    fn fresh_memory_keeps_full_confidence() {
        // now == last_used: no elapsed time, no decay.
        assert_eq!(
            decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 1000, 1000, HL),
            FULL_CONFIDENCE_MICROS
        );
        // now before last_used (clock skew): never amplifies.
        assert_eq!(
            decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 2000, 1000, HL),
            FULL_CONFIDENCE_MICROS
        );
    }

    #[test]
    fn one_half_life_halves_exactly() {
        let d = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, HL, HL);
        assert_eq!(d, 500_000, "one half-life -> exactly half");
    }

    #[test]
    fn two_and_three_half_lives() {
        assert_eq!(
            decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, 2 * HL, HL),
            250_000
        );
        assert_eq!(
            decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, 3 * HL, HL),
            125_000
        );
    }

    #[test]
    fn half_way_through_a_half_life_interpolates_between_one_and_half() {
        // At frac = HL/2 the linear factor is (2HL - HL/2)/(2HL) = 3/4.
        let d = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, HL / 2, HL);
        assert_eq!(d, 750_000, "linear interp of the first halving");
    }

    #[test]
    fn monotonically_non_increasing_over_time() {
        let mut prev = i64::MAX;
        for k in 0..12 {
            let t = k * HL / 3;
            let d = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, t, HL);
            assert!(d <= prev, "decay must never increase: {d} > {prev} at {t}");
            prev = d;
        }
    }

    #[test]
    fn very_old_decays_to_zero_without_overflow() {
        let d = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 0, 1_000 * HL, HL);
        assert_eq!(d, 0, "ancient memory decays to nothing");
    }

    #[test]
    fn partial_base_confidence_scales() {
        // A memory already at half confidence decays from there.
        let d = decayed_confidence_micros(500_000, 0, HL, HL);
        assert_eq!(d, 250_000);
    }

    #[test]
    fn zero_or_negative_half_life_is_a_noop() {
        assert_eq!(decayed_confidence_micros(600_000, 0, 10 * HL, 0), 600_000);
        assert_eq!(decayed_confidence_micros(600_000, 0, 10 * HL, -5), 600_000);
    }

    #[test]
    fn deterministic_across_repeated_calls() {
        let a = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 100, 100 + 7 * HL / 5, HL);
        let b = decayed_confidence_micros(FULL_CONFIDENCE_MICROS, 100, 100 + 7 * HL / 5, HL);
        assert_eq!(a, b, "same inputs -> same micros, always");
    }
}
