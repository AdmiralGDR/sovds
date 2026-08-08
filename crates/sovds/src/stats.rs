// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Online statistics: tiny, exact estimators that learn a baseline from a stream
//! without storing history.
//!
//! Everything here is **integer fixed-point** on purpose. Floating point is
//! avoided so results are bit-identical on every machine (the principle) and free of
//! NaN/inf edge cases (the principle), and every operation is a checked/saturating
//! method call — never a bare operator — so nothing can overflow or divide by
//! zero (the principle: no undefined arithmetic). Each estimator uses O(1) memory and
//! O(1) work per sample (the principle: bounded execution; the principle: data-oriented).

/// Values are stored multiplied by this scale to keep sub-unit precision using
/// only integers.
pub const SCALE: u64 = 1000;

/// Exponentially weighted moving average.
///
/// `update` folds a raw sample in as `new = ((w-1)*old + x) / w`, so a larger
/// weight tracks more slowly. The estimate is `None` until the first sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ewma {
    /// Smoothed value in fixed-point (real * SCALE), or `None` before any sample.
    value: Option<u64>,
    /// Smoothing denominator `w` (>= 1).
    weight: u64,
}

impl Ewma {
    /// Create an EWMA with smoothing denominator `weight` (coerced to >= 1).
    #[must_use]
    pub const fn new(weight: u64) -> Self {
        Self {
            value: None,
            weight: if weight == 0 { 1 } else { weight },
        }
    }

    /// Fold in a raw (unscaled) sample.
    pub fn update(&mut self, sample: u64) {
        let scaled = sample.saturating_mul(SCALE);
        let next = match self.value {
            None => scaled,
            Some(old) => {
                let keep = self.weight.saturating_sub(1);
                old.saturating_mul(keep)
                    .saturating_add(scaled)
                    .checked_div(self.weight)
                    .unwrap_or(old)
            }
        };
        self.value = Some(next);
    }

    /// Current estimate in real (unscaled, rounded-down) units.
    #[must_use]
    pub fn get(&self) -> Option<u64> {
        self.value
            .map(|scaled| scaled.checked_div(SCALE).unwrap_or(0))
    }

    /// Whether any sample has been folded in yet.
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.value.is_some()
    }
}

/// One-sided CUSUM change detector for an *upward* shift.
///
/// This is the detector for the "works then dies" throttle: a value steps up and
/// stays up. CUSUM accumulates how far samples run above their expected mean
/// (beyond a noise slack `k`) and alarms once that accumulation crosses `h`. It
/// reacts to a sustained shift far faster than a fixed threshold, and needs no
/// history. All math is saturating on `i64`, so drift is bounded and signed
/// without ever wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cusum {
    /// Accumulated positive drift, fixed-point. Bounded to `[0, ceiling]`.
    sum: i64,
    /// Noise slack `k`, fixed-point: drift below this is ignored.
    slack: i64,
    /// Alarm threshold `h`, fixed-point.
    threshold: i64,
    /// Upper bound on `sum`, fixed-point. Without it, a long or large upward
    /// excursion winds the accumulator up without limit, and it then takes just as
    /// many below-baseline samples to unwind — so the alarm *latches* long after
    /// the signal has recovered (measured on real data: a fast path, value well
    /// under baseline, still read alarmed for 30+ cycles). A bounded CUSUM caps the
    /// wind-up at one extra threshold above the alarm line, so recovery drains the
    /// alarm within a couple of samples. It does not change *detection* — `sum`
    /// still crosses `threshold` on exactly the same sample — only how promptly the
    /// alarm clears afterwards (the principle: bounded state).
    ceiling: i64,
}

impl Cusum {
    /// Create a detector with noise slack `slack_units` and alarm threshold
    /// `threshold_units`, both in real (unscaled) units.
    #[must_use]
    pub const fn new(slack_units: u64, threshold_units: u64) -> Self {
        let threshold = threshold_units.saturating_mul(SCALE) as i64;
        Self {
            sum: 0,
            slack: slack_units.saturating_mul(SCALE) as i64,
            threshold,
            // One extra threshold of head-room above the alarm line: enough that a
            // brief post-alarm blip does not instantly clear the alarm, but bounded
            // so a sustained fault cannot wind up an unbounded recovery debt.
            ceiling: threshold.saturating_mul(2),
        }
    }

    /// Fold in `sample` given its `expected` mean (both real units). Returns
    /// whether the accumulated upward drift has crossed the threshold.
    pub fn update(&mut self, sample: u64, expected: u64) -> bool {
        let x = sample.saturating_mul(SCALE) as i64;
        let mean = expected.saturating_mul(SCALE) as i64;
        let excess = x.saturating_sub(mean).saturating_sub(self.slack);
        // Clamp into [0, ceiling]: the floor keeps drift non-negative, the ceiling
        // bounds wind-up so recovery cannot be latched (see `ceiling`).
        self.sum = self.sum.saturating_add(excess).max(0).min(self.ceiling);
        self.alarmed()
    }

    /// Clear the accumulated drift (e.g. after acting on the alarm).
    pub fn reset(&mut self) {
        self.sum = 0;
    }

    /// Whether the detector is currently in alarm.
    #[must_use]
    pub fn alarmed(&self) -> bool {
        self.sum > self.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::{Cusum, Ewma};

    #[test]
    fn ewma_is_empty_before_the_first_sample() {
        let e = Ewma::new(8);
        assert!(!e.is_primed());
        assert_eq!(e.get(), None);
    }

    #[test]
    fn ewma_seeds_on_first_sample_then_tracks_slowly() {
        let mut e = Ewma::new(4);
        e.update(100);
        assert_eq!(e.get(), Some(100));
        // new = (3*100 + 200)/4 = 125
        e.update(200);
        assert_eq!(e.get(), Some(125));
    }

    #[test]
    fn ewma_converges_toward_a_constant_stream() {
        let mut e = Ewma::new(4);
        for _ in 0..50 {
            e.update(80);
        }
        assert_eq!(e.get(), Some(80));
    }

    #[test]
    fn cusum_stays_quiet_on_a_steady_stream() {
        let mut c = Cusum::new(20, 100);
        let mut alarm = false;
        for _ in 0..50 {
            alarm |= c.update(50, 50);
        }
        assert!(!alarm, "no upward shift, no alarm");
    }

    #[test]
    fn cusum_fires_on_a_sustained_upward_shift() {
        let mut c = Cusum::new(20, 100);
        // Baseline mean stays 50 while samples jump to 150: excess ~80 per step,
        // so it should cross the threshold of 100 within two samples.
        assert!(!c.update(150, 50));
        assert!(c.update(150, 50));
    }

    #[test]
    fn cusum_clears_promptly_after_a_long_excursion() {
        // The anti-latch property (a real production defect): a long, large upward
        // excursion must not wind the accumulator up so far that recovery is
        // latched. Run a sustained fault, then feed samples back at baseline and
        // assert the alarm clears within a few cycles — not the ~30+ it took before
        // the accumulator was bounded.
        let mut c = Cusum::new(20, 100);
        for _ in 0..100 {
            c.update(300, 50); // far above baseline, for a long time
        }
        assert!(c.alarmed(), "a sustained upward shift must alarm");
        let mut cleared_at = None;
        for step in 1..=5 {
            // Back to baseline: each below-slack sample drains the bounded debt.
            if !c.update(50, 50) {
                cleared_at = Some(step);
                break;
            }
        }
        assert!(
            cleared_at.is_some(),
            "alarm latched: still firing 5 cycles after full recovery"
        );
    }

    #[test]
    fn cusum_can_be_reset() {
        let mut c = Cusum::new(20, 100);
        c.update(150, 50);
        c.update(150, 50);
        assert!(c.alarmed());
        c.reset();
        assert!(!c.alarmed());
    }
}
