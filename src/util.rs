//! util — the shared substrate: injected time, strict RFC3339, and (as later
//! beads land) FNV-1a hashing and fixed-point integer helpers.
//!
//! # Determinism conventions (crate law, inherited not re-argued)
//!
//! - No `HashMap` iteration order may ever reach an output path. Use
//!   `BTreeMap` or an explicit sort.
//! - No wall clock and no randomness inside serialization or scoring paths.
//!   Time enters the crate exactly once: through the [`Clock`] trait
//!   ([`SystemClock`] in production, [`FixedClock`] in tests).
//! - All emitted text uses LF line endings only, with exactly one trailing
//!   newline on files.
//! - No floats in scoring. Fixed-point i64 micro-units only.
//!
//! # The `GHOSTIE_TEST_CLOCK` hook
//!
//! When the environment variable `GHOSTIE_TEST_CLOCK` is set to a strict
//! RFC3339 UTC instant (e.g. `2026-07-13T12:00:00Z`), [`resolve_clock`]
//! yields a [`FixedClock`] at that instant instead of wall time. This is what
//! lets the gate's byte-stability step build two stores in two runs and
//! demand byte-identical trees, created timestamps included. The hook is
//! honored only when set, is deliberately not advertised in help output, and
//! a malformed value is a hard error (a silent fallback to wall time would
//! break byte-stability mysteriously).

use crate::error::{Error, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Injected time source. Seconds since the Unix epoch, UTC.
///
/// Second precision is deliberate: the store format records `created` at
/// second precision, and coarser time means fewer ways for two runs to
/// differ.
pub trait Clock {
    /// Current time as seconds since 1970-01-01T00:00:00Z.
    fn now_epoch_seconds(&self) -> i64;
}

/// Production clock: reads the system wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch_seconds(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            // Pre-1970 system clock: represent as negative seconds.
            Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
        }
    }
}

/// Test double: always reports the same instant.
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_epoch_seconds(&self) -> i64 {
        self.0
    }
}

/// Resolve the process clock, honoring the `GHOSTIE_TEST_CLOCK` hook.
///
/// Reads the environment exactly once per call; all logic lives in
/// [`resolve_clock_from`] so tests can cover both paths without mutating
/// process-global env (which is unsafe in edition 2024).
pub fn resolve_clock() -> Result<Box<dyn Clock>> {
    let v = std::env::var("GHOSTIE_TEST_CLOCK").ok();
    resolve_clock_from(v.as_deref())
}

/// Clock resolution logic: `None` -> [`SystemClock`]; `Some(rfc3339)` ->
/// [`FixedClock`] at that instant; a malformed value is an error.
pub fn resolve_clock_from(test_clock: Option<&str>) -> Result<Box<dyn Clock>> {
    match test_clock {
        None => Ok(Box::new(SystemClock)),
        Some(s) => Ok(Box::new(FixedClock(parse_rfc3339_utc(s)?))),
    }
}

/// Days in each month of a non-leap year, 1-indexed by month.
const DAYS_IN_MONTH: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    if m == 2 && is_leap_year(y) {
        29
    } else {
        DAYS_IN_MONTH[m as usize]
    }
}

/// Days from the epoch (1970-01-01) to the given civil date.
/// Standard integer-only civil-calendar algorithm (Hinnant's
/// `days_from_civil`), valid for all years in our supported range.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: epoch day count -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse a strict RFC3339 UTC timestamp: exactly `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Second precision, `Z` suffix only — no offsets, no fractional seconds, no
/// lowercase `t`/`z`. This is the one timestamp shape the store format
/// permits; strictness is what makes byte-stability checkable.
pub fn parse_rfc3339_utc(s: &str) -> Result<i64> {
    let err = |reason: &'static str| Error::InvalidTimestamp {
        value: s.to_string(),
        reason,
    };
    let b = s.as_bytes();
    if b.len() != 20 {
        return Err(err("must be exactly YYYY-MM-DDTHH:MM:SSZ (20 chars)"));
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return Err(err("separators must be -, -, T, :, :"));
    }
    if b[19] != b'Z' {
        return Err(err("must end in Z (UTC only)"));
    }
    let num = |range: std::ops::Range<usize>, what: &'static str| -> Result<i64> {
        let mut v: i64 = 0;
        for &c in &b[range] {
            if !c.is_ascii_digit() {
                return Err(err(what));
            }
            v = v * 10 + i64::from(c - b'0');
        }
        Ok(v)
    };
    let y = num(0..4, "year must be 4 digits")?;
    let mo = num(5..7, "month must be 2 digits")?;
    let d = num(8..10, "day must be 2 digits")?;
    let h = num(11..13, "hour must be 2 digits")?;
    let mi = num(14..16, "minute must be 2 digits")?;
    let sec = num(17..19, "second must be 2 digits")?;
    if !(1..=12).contains(&mo) {
        return Err(err("month out of range 01-12"));
    }
    if d < 1 || d > days_in_month(y, mo) {
        return Err(err("day out of range for month"));
    }
    if h > 23 {
        return Err(err("hour out of range 00-23"));
    }
    if mi > 59 {
        return Err(err("minute out of range 00-59"));
    }
    if sec > 59 {
        return Err(err(
            "second out of range 00-59 (leap seconds not supported)",
        ));
    }
    Ok(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

/// Format epoch seconds as strict RFC3339 UTC: `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Inverse of [`parse_rfc3339_utc`]; round-trips exactly.
pub fn format_rfc3339_utc(epoch_seconds: i64) -> String {
    let days = epoch_seconds.div_euclid(86400);
    let secs = epoch_seconds.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The crate-wide fixed-point scale: one unit = one micro (1e-6). All
/// scoring math is i64 micro-units; floats are banned in scoring paths and
/// the gate greps for them.
pub const SCALE: i64 = 1_000_000;

/// ln(2) in micro-units (0.6931471805... rounded half-up).
pub const LN2_MICROS: i64 = 693_147;

/// `a * b / c` in i64 with i128 intermediates and round-half-up, the
/// crate's one rounding rule. Requires `c > 0` and non-negative `a * b`.
pub fn mul_div_round(a: i64, b: i64, c: i64) -> i64 {
    debug_assert!(c > 0, "mul_div_round: divisor must be positive");
    let prod = i128::from(a) * i128::from(b);
    debug_assert!(prod >= 0, "mul_div_round: negative products unsupported");
    let c = i128::from(c);
    ((prod + c / 2) / c) as i64
}

/// Fixed-point natural log of the rational `num / den`, in micro-units.
/// Requires `num > 0` and `den > 0`; the result may be negative.
///
/// Algorithm (reproducible by reading this):
/// 1. Range-reduce the rational to `m = num/den` in `[1, 2)` by shifting:
///    `ln(x) = k*ln(2) + ln(m)`.
/// 2. `ln(m) = 2 * atanh(z)` with `z = (m-1)/(m+1) = (num-den)/(num+den)`;
///    for `m` in `[1,2)`, `z` is in `[0, 1/3]`.
/// 3. atanh series `z + z^3/3 + z^5/5 + ...`, exactly 12 terms (odd powers
///    through z^23), computed in i128 NANO-units (1e9) with round-half-up,
///    rounded to micros exactly once at the end — so accumulated series
///    rounding stays three orders of magnitude below the result precision.
///    With `z <= 1/3` the truncation error is far below one nano.
pub fn ln_micros(num: i64, den: i64) -> i64 {
    assert!(num > 0 && den > 0, "ln_micros: arguments must be positive");
    const NANO: i128 = 1_000_000_000;
    const LN2_NANOS: i128 = 693_147_181; // ln 2 = 0.6931471805599453
    let (mut num, mut den) = (i128::from(num), i128::from(den));
    let mut k: i128 = 0;
    while num >= 2 * den {
        den <<= 1;
        k += 1;
    }
    while num < den {
        num <<= 1;
        k -= 1;
    }
    // z in nano-units, round half up (all quantities non-negative here).
    let z = {
        let n = (num - den) * NANO;
        let d = num + den;
        (n + d / 2) / d
    };
    let z2 = (z * z + NANO / 2) / NANO;
    let mut term = z; // z^(2i+1) in nanos
    let mut sum = z; // series accumulator in nanos
    let mut n: i128 = 1;
    for _ in 0..11 {
        n += 2;
        term = (term * z2 + NANO / 2) / NANO;
        sum += (term + n / 2) / n;
    }
    let nanos = k * LN2_NANOS + 2 * sum;
    // Round nanos -> micros, half away from zero (negatives allowed here).
    let half = if nanos >= 0 { 500 } else { -500 };
    ((nanos + half) / 1000) as i64
}

/// FNV-1a 64-bit hash. Used for index content hashes and (in the capture
/// phase) idempotent session-memory disambiguators. Stable by definition:
/// same bytes, same u64, every platform, forever.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// FNV-1a 64 as the fixed-width lowercase hex string used on disk.
pub fn fnv1a64_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a64(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ln_micros_known_values() {
        assert_eq!(ln_micros(1, 1), 0);
        assert_eq!(ln_micros(2, 1), 693_147); // ln 2
        assert_eq!(ln_micros(1, 2), -693_147); // ln 1/2
        assert_eq!(ln_micros(10, 1), 2_302_585); // ln 10 = 2.302585093
        assert_eq!(ln_micros(3, 2), 405_465); // ln 1.5 = 0.405465108
        assert_eq!(ln_micros(1_000_000, 1), 13_815_511); // 6 ln 10 = 13.81551056
        assert_eq!(ln_micros(7, 3), 847_298); // ln(7/3) = 0.8472978604
        // BM25's idf shape: ln((2N+2)/(2df+1)).
        assert_eq!(ln_micros(6, 3), 693_147); // N=2, df=1
    }

    #[test]
    fn mul_div_round_rounds_half_up() {
        assert_eq!(mul_div_round(1, 1, 2), 1); // 0.5 -> 1
        assert_eq!(mul_div_round(1, 1, 3), 0); // 0.33 -> 0
        assert_eq!(mul_div_round(2, 1, 3), 1); // 0.66 -> 1
        assert_eq!(mul_div_round(693_147, 4_400_000, 2_900_000), 1_051_671);
        // i128 intermediates: no overflow near i64 limits.
        assert_eq!(mul_div_round(i64::MAX / 2, 4, 2), i64::MAX - 1);
    }

    #[test]
    fn fnv1a64_published_test_vectors() {
        // Vectors from the FNV reference material.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(fnv1a64_hex(b""), "cbf29ce484222325");
    }

    #[test]
    fn fixed_clock_reports_its_instant() {
        let c = FixedClock(1_752_408_000);
        assert_eq!(c.now_epoch_seconds(), 1_752_408_000);
        assert_eq!(c.now_epoch_seconds(), 1_752_408_000);
    }

    #[test]
    fn system_clock_is_plausibly_now() {
        let t = SystemClock.now_epoch_seconds();
        // After 2020-01-01, before 2100-01-01. Catches unit mistakes (ms vs s).
        assert!(t > 1_577_836_800, "system clock too early: {t}");
        assert!(t < 4_102_444_800, "system clock too late: {t}");
    }

    #[test]
    fn resolve_clock_without_hook_is_system_time() {
        let c = resolve_clock_from(None).expect("no hook must resolve");
        let t = c.now_epoch_seconds();
        assert!(t > 1_577_836_800, "expected wall time, got {t}");
    }

    #[test]
    fn resolve_clock_with_hook_is_fixed_at_that_instant() {
        let c = resolve_clock_from(Some("2026-01-02T03:04:05Z")).expect("valid hook value");
        let expected = parse_rfc3339_utc("2026-01-02T03:04:05Z").unwrap();
        assert_eq!(c.now_epoch_seconds(), expected);
        assert_eq!(c.now_epoch_seconds(), expected, "must be frozen, not wall");
    }

    #[test]
    fn resolve_clock_with_malformed_hook_is_a_hard_error() {
        for bad in [
            "yesterday",
            "2026-01-02 03:04:05Z",
            "2026-01-02T03:04:05+00:00",
            "",
        ] {
            assert!(
                resolve_clock_from(Some(bad)).is_err(),
                "malformed hook {bad:?} must error, not fall back to wall time"
            );
        }
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_rfc3339_utc("1970-01-02T00:00:00Z").unwrap(), 86400);
        // Independently checkable: date -u -d @1752408000
        assert_eq!(
            parse_rfc3339_utc("2025-07-13T12:00:00Z").unwrap(),
            1_752_408_000
        );
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_752_408_000), "2025-07-13T12:00:00Z");
    }

    #[test]
    fn rfc3339_round_trip_including_leap_day() {
        for s in [
            "1970-01-01T00:00:00Z",
            "1999-12-31T23:59:59Z",
            "2000-02-29T12:34:56Z", // leap day, century leap year
            "2024-02-29T00:00:00Z", // leap day
            "2026-07-13T08:09:10Z",
            "2100-01-01T00:00:00Z",
        ] {
            let t = parse_rfc3339_utc(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(format_rfc3339_utc(t), s, "round trip of {s}");
        }
    }

    #[test]
    fn rfc3339_rejects_malformed() {
        for bad in [
            "2026-02-29T00:00:00Z",      // not a leap year
            "2100-02-29T00:00:00Z",      // century non-leap year (catches naive %4)
            "2026-13-01T00:00:00Z",      // month 13
            "2026-00-01T00:00:00Z",      // month 0
            "2026-01-32T00:00:00Z",      // day 32
            "2026-01-00T00:00:00Z",      // day 0
            "2026-01-01T24:00:00Z",      // hour 24
            "2026-01-01T00:60:00Z",      // minute 60
            "2026-01-01T00:00:60Z",      // leap second
            "2026-01-01T00:00:00z",      // lowercase z
            "2026-01-01t00:00:00Z",      // lowercase t
            "2026-01-01T00:00:00",       // missing Z
            "2026-01-01T00:00:00.000Z",  // fractional seconds
            "2026-01-01T00:00:00+00:00", // offset form
            "26-01-01T00:00:00Z",        // short year
            "",
        ] {
            assert!(parse_rfc3339_utc(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
