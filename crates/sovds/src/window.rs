// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Streaming **temporal features** — give a per-record model the history it would
//! otherwise be blind to, without breaking causality.
//!
//! A raw record is a snapshot: `tunnel latency = 440`. But whether 440 is *healthy*
//! depends on this channel's own recent baseline — a node that normally sits at 120
//! is degraded at 440, one that normally sits at 430 is fine. A model trained on the
//! bare snapshot cannot know that; the adaptive layer that *does* know it keeps a
//! running baseline. This module rebuilds those same baseline-relative signals as
//! plain features, so a stateless learner (a [`crate::export`] table, a decision
//! tree) can see what the online adaptive layer sees.
//!
//! For each raw channel it emits three columns:
//! - the **raw** value (unchanged),
//! - the **deviation** from the channel's causal EWMA baseline (`value − baseline`;
//!   the "how far from normal *for this channel*" signal the adaptive layer thresholds
//!   on), and
//! - the **rate** of change since the previous measured sample (`value − previous`).
//!
//! Everything is integer and causal — each column depends only on the current and
//! past samples, never the future — so the transform is safe to compute over a whole
//! corpus and *then* split into train/test: the state that crosses the split is
//! exactly the state an online consumer would already hold (deterministic, bounded — one EWMA and one previous value per channel).
//!
//! **Missing values.** A channel reports `-1` (any negative) for "not measured".
//! Those samples do not update the baseline (they would poison it) and emit a
//! deviation and rate of `0` — an honest "no information", never a fabricated jump.
//!
//! The transform is domain-agnostic: it operates on the flat signal vector any
//! [`crate::domain::Domain`] already produces, so every domain gains temporal
//! features for free.

use crate::stats::Ewma;
use core::fmt::Write as _;
use std::string::String;
use std::vec::Vec;

/// How many feature columns each raw channel expands into (raw, deviation, rate).
pub const COLUMNS_PER_CHANNEL: usize = 3;

/// Tuning for the temporal transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowConfig {
    /// EWMA weight for the baseline, in per-mille (`crate::stats::SCALE`). Smaller =
    /// slower, longer-memory baseline. `200` ≈ 0.2, a few-minutes memory at a
    /// one-per-minute cadence.
    pub ewma_weight: u64,
}

impl WindowConfig {
    /// Sensible default: a slow baseline (weight 0.2) that tracks the channel's
    /// normal level without chasing every spike.
    #[must_use]
    pub const fn new() -> Self {
        Self { ewma_weight: 200 }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A stateful streaming transform: feed it one raw signal vector at a time (in
/// chronological order) and it appends the expanded temporal features. Call
/// [`WindowFeatures::reset`] at a run boundary so history never leaks across an
/// independent collection run.
#[derive(Debug, Clone)]
pub struct WindowFeatures {
    ewma: Vec<Ewma>,
    prev: Vec<Option<i64>>,
    cfg: WindowConfig,
}

impl WindowFeatures {
    /// A fresh transform with no history.
    #[must_use]
    pub fn new(cfg: WindowConfig) -> Self {
        Self {
            ewma: Vec::new(),
            prev: Vec::new(),
            cfg,
        }
    }

    /// Drop all per-channel history (baselines and previous values). Call this at
    /// the start of each independent run so a new run's first sample is not compared
    /// against the previous run's baseline.
    pub fn reset(&mut self) {
        self.ewma.clear();
        self.prev.clear();
    }

    /// Ensure per-channel state exists for `n` channels.
    fn ensure(&mut self, n: usize) {
        while self.ewma.len() < n {
            self.ewma.push(Ewma::new(self.cfg.ewma_weight));
        }
        while self.prev.len() < n {
            self.prev.push(None);
        }
    }

    /// Transform one raw signal vector, pushing `COLUMNS_PER_CHANNEL` values per
    /// channel (raw, deviation-from-baseline, rate) onto `out`. Updates the internal
    /// baseline with each measured sample. Total and panic-free.
    pub fn push(&mut self, raw: &[i64], out: &mut Vec<i64>) {
        self.ensure(raw.len());
        for (i, &value) in raw.iter().enumerate() {
            let measured = value >= 0;

            // Baseline BEFORE this sample: what "normal" was up to now. On the first
            // measured sample there is no prior baseline, so deviation is 0.
            let baseline = self
                .ewma
                .get(i)
                .and_then(Ewma::get)
                .and_then(|b| i64::try_from(b).ok());
            let dev = match (measured, baseline) {
                (true, Some(b)) => value.saturating_sub(b),
                _ => 0,
            };
            let rate = match (measured, self.prev.get(i).copied().flatten()) {
                (true, Some(p)) => value.saturating_sub(p),
                _ => 0,
            };

            out.push(value);
            out.push(dev);
            out.push(rate);

            // Update history only with real measurements.
            if measured {
                if let Some(e) = self.ewma.get_mut(i) {
                    if let Ok(u) = u64::try_from(value) {
                        e.update(u);
                    }
                }
                if let Some(p) = self.prev.get_mut(i) {
                    *p = Some(value);
                }
            }
        }
    }

    /// Convenience: transform one vector into a fresh `Vec`.
    #[must_use]
    pub fn transform(&mut self, raw: &[i64]) -> Vec<i64> {
        let mut out = Vec::new();
        self.push(raw, &mut out);
        out
    }
}

/// Build the expanded feature names for a set of raw channel names: each `name`
/// becomes `name`, `name_dev`, `name_rate` (matching [`WindowFeatures::push`] order).
#[must_use]
pub fn feature_names(raw_names: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    for &name in raw_names {
        names.push(String::from(name));
        let mut dev = String::new();
        let _ = write!(dev, "{name}_dev");
        names.push(dev);
        let mut rate = String::new();
        let _ = write!(rate, "{name}_rate");
        names.push(rate);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{feature_names, WindowConfig, WindowFeatures};
    use std::vec;

    #[test]
    fn first_sample_has_zero_deviation_and_rate() {
        let mut w = WindowFeatures::new(WindowConfig::new());
        let out = w.transform(&[440, 1]);
        // raw, dev, rate per channel; first sample => dev=0, rate=0.
        assert_eq!(out, vec![440, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn a_rise_above_baseline_shows_positive_deviation() {
        let mut w = WindowFeatures::new(WindowConfig::new());
        // Prime a low baseline.
        for _ in 0..20 {
            let _ = w.transform(&[120]);
        }
        // A jump to 480 must read as strongly positive deviation and rate.
        let out = w.transform(&[480]);
        let dev = out.get(1).copied().unwrap_or(0);
        let rate = out.get(2).copied().unwrap_or(0);
        assert!(
            dev > 200,
            "deviation {dev} should expose the jump above baseline"
        );
        assert_eq!(rate, 360, "rate is 480 - previous 120");
    }

    #[test]
    fn unmeasured_samples_do_not_poison_the_baseline() {
        let mut w = WindowFeatures::new(WindowConfig::new());
        for _ in 0..20 {
            let _ = w.transform(&[120]);
        }
        // A "not measured" (-1) sample emits zero dev/rate and leaves the baseline.
        let gap = w.transform(&[-1]);
        assert_eq!(gap.get(1).copied(), Some(0), "unmeasured dev is 0");
        assert_eq!(gap.get(2).copied(), Some(0), "unmeasured rate is 0");
        // The following measured sample is still compared against the 120 baseline,
        // not against the -1.
        let after = w.transform(&[121]);
        let dev = after.get(1).copied().unwrap_or(0);
        assert!(
            dev.abs() < 10,
            "baseline survived the gap (dev {dev} near 0)"
        );
    }

    #[test]
    fn reset_clears_history_between_runs() {
        let mut w = WindowFeatures::new(WindowConfig::new());
        for _ in 0..20 {
            let _ = w.transform(&[120]);
        }
        w.reset();
        // After reset the first sample of the new run is a fresh baseline => dev 0.
        let out = w.transform(&[480]);
        assert_eq!(out.get(1).copied(), Some(0), "reset zeroes the baseline");
    }

    #[test]
    fn names_expand_three_per_channel() {
        let names = feature_names(&["lat", "up"]);
        assert_eq!(
            names,
            vec!["lat", "lat_dev", "lat_rate", "up", "up_dev", "up_rate"]
        );
    }
}
