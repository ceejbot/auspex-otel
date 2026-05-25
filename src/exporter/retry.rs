//! Retry timing for the OTLP/HTTP exporter (Task 6.2).
//!
//! This module is intentionally small and **pure**: it computes the backoff
//! delay for a given retry attempt and nothing else. The decision of *whether*
//! a particular HTTP response or transport error is retryable, and the actual
//! sleep + re-send loop, live in the concrete exporter — that keeps the timing
//! math free of `reqwest` and randomness so it can be unit-tested directly.
//!
//! Scheme: **full jitter** exponential backoff. For retry attempt `n` (1-based)
//! the ceiling is `min(cap, base * 2^(n-1))` and the actual delay is a uniform
//! sample in `[0, ceiling]`. Full jitter is the AWS-recommended scheme; it
//! spreads retries out and avoids the synchronized-retry thundering herd that
//! plain or equal-jitter backoff still permits.

// This is a private module; `pub(crate)` is the intended in-crate visibility.
#![allow(clippy::redundant_pub_crate)]

use std::time::Duration;

/// Backoff schedule for export retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// Delay ceiling for the first retry (doubles each subsequent attempt).
    base: Duration,
    /// Absolute ceiling on any single delay.
    cap: Duration,
    /// How many retries follow the initial attempt.
    max_retries: u32,
}

impl Default for Backoff {
    /// The v0.1 policy: base 100 ms, cap 5 s, up to 2 retries
    /// (3 total attempts).
    fn default() -> Self {
        Self {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(5),
            max_retries: 2,
        }
    }
}

impl Backoff {
    /// Retries permitted after the initial attempt.
    pub(crate) const fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Delay before retry `attempt` (1-based), or `None` once the retry budget
    /// is exhausted (`attempt > max_retries`).
    ///
    /// `jitter` is a uniform fraction in `[0, 1)` supplied by the caller, so
    /// the schedule is deterministic under test. Values outside the range
    /// are clamped.
    pub(crate) fn delay_for(&self, attempt: u32, jitter: f64) -> Option<Duration> {
        if attempt == 0 || attempt > self.max_retries {
            return None;
        }

        // ceiling = base * 2^(attempt-1), saturating at `cap` (and on overflow).
        let multiplier = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
        let ceiling = self
            .base
            .checked_mul(multiplier)
            .map_or(self.cap, |raw| raw.min(self.cap));

        Some(ceiling.mul_f64(jitter.clamp(0.0, 1.0)))
    }
}

/// A uniform jitter fraction in `[0, 1)` drawn from the OS RNG.
///
/// Used by the exporter to feed [`Backoff::delay_for`] in production; tests
/// pass a fixed fraction instead.
#[allow(clippy::cast_precision_loss)] // jitter tolerates the mantissa rounding
pub(crate) fn jitter_fraction() -> f64 {
    let mut bytes = [0u8; 8];
    // If the RNG ever fails, fall back to the full ceiling (jitter = ~1.0):
    // a slightly longer wait is harmless, and this path is effectively never hit.
    if getrandom::fill(&mut bytes).is_err() {
        return 1.0 - f64::EPSILON;
    }
    // Map u64 -> [0, 1) by dividing by 2^64.
    (u64::from_le_bytes(bytes) as f64) / (2.0_f64.powi(64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_v0_1_spec() {
        let b = Backoff::default();
        assert_eq!(b.base, Duration::from_millis(100));
        assert_eq!(b.cap, Duration::from_secs(5));
        assert_eq!(b.max_retries(), 2);
    }

    #[test]
    fn ceiling_grows_exponentially_with_max_jitter() {
        let b = Backoff::default();
        // jitter = 1.0 -> delay equals the ceiling for that attempt.
        assert_eq!(b.delay_for(1, 1.0), Some(Duration::from_millis(100)));
        assert_eq!(b.delay_for(2, 1.0), Some(Duration::from_millis(200)));
    }

    #[test]
    fn ceiling_is_clamped_to_cap() {
        let b = Backoff {
            base: Duration::from_secs(4),
            cap: Duration::from_secs(5),
            max_retries: 5,
        };
        // attempt 1: 4s (<= cap), attempt 2: 8s -> clamped to 5s.
        assert_eq!(b.delay_for(1, 1.0), Some(Duration::from_secs(4)));
        assert_eq!(b.delay_for(2, 1.0), Some(Duration::from_secs(5)));
        assert_eq!(b.delay_for(3, 1.0), Some(Duration::from_secs(5)));
    }

    #[test]
    fn zero_jitter_yields_zero_delay() {
        let b = Backoff::default();
        assert_eq!(b.delay_for(1, 0.0), Some(Duration::ZERO));
    }

    #[test]
    fn jitter_scales_within_the_ceiling() {
        let b = Backoff::default();
        // Half jitter on the first attempt: 50 ms of the 100 ms ceiling.
        assert_eq!(b.delay_for(1, 0.5), Some(Duration::from_millis(50)));
    }

    #[test]
    fn out_of_range_jitter_is_clamped() {
        let b = Backoff::default();
        assert_eq!(b.delay_for(1, 2.0), Some(Duration::from_millis(100)));
        assert_eq!(b.delay_for(1, -1.0), Some(Duration::ZERO));
    }

    #[test]
    fn returns_none_once_budget_exhausted() {
        let b = Backoff::default();
        assert_eq!(b.delay_for(0, 1.0), None);
        assert_eq!(
            b.delay_for(3, 1.0),
            None,
            "max_retries is 2, so attempt 3 is out of budget"
        );
    }

    #[test]
    fn jitter_fraction_is_in_unit_interval() {
        for _ in 0..1_000 {
            let f = jitter_fraction();
            assert!((0.0..1.0).contains(&f), "jitter {f} out of [0,1)");
        }
    }
}
