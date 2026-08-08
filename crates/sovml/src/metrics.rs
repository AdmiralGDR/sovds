// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Classification metrics — how you *know* a model got better (the principle: metrics
//! before theories; you may not claim an improvement you did not measure).
//!
//! Everything is integer, reported in **per-mille** (‰, ×1000) so a fraction like
//! 0.936 is the integer `936` — deterministic and float-free (the principle). The
//! headline number for an imbalanced problem is [`Metrics::macro_f1_pm`]: the mean
//! F1 across classes, which a model cannot inflate by ignoring the rare classes.

use crate::tree::MAX_CLASSES;

/// Per-mille scale (a rate of 1.0 is `1000`).
pub const PER_MILLE: u64 = 1000;

/// A confusion matrix and the metrics derived from it.
#[derive(Debug, Clone)]
pub struct Metrics {
    /// `confusion[t][p]` = count of samples whose true class is `t`, predicted `p`.
    confusion: [[u64; MAX_CLASSES]; MAX_CLASSES],
    n_classes: usize,
}

impl Metrics {
    /// A fresh, empty confusion matrix over `n_classes` (clamped to [`MAX_CLASSES`]).
    #[must_use]
    pub fn new(n_classes: usize) -> Self {
        Self {
            confusion: [[0u64; MAX_CLASSES]; MAX_CLASSES],
            n_classes: n_classes.clamp(1, MAX_CLASSES),
        }
    }

    /// Record one (truth, prediction) pair.
    pub fn record(&mut self, truth: u32, pred: u32) {
        let t = (truth as usize).min(self.n_classes.saturating_sub(1));
        let p = (pred as usize).min(self.n_classes.saturating_sub(1));
        if let Some(cell) = self.confusion.get_mut(t).and_then(|row| row.get_mut(p)) {
            *cell = cell.saturating_add(1);
        }
    }

    /// Total samples recorded.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.confusion
            .iter()
            .flatten()
            .fold(0u64, |a, &c| a.saturating_add(c))
    }

    /// Overall accuracy in per-mille.
    #[must_use]
    pub fn accuracy_pm(&self) -> u64 {
        let total = self.total();
        let correct = (0..self.n_classes).fold(0u64, |a, c| {
            a.saturating_add(
                self.confusion
                    .get(c)
                    .and_then(|r| r.get(c))
                    .copied()
                    .unwrap_or(0),
            )
        });
        ratio_pm(correct, total)
    }

    /// True positives for `class` (the diagonal cell).
    fn tp(&self, class: usize) -> u64 {
        self.confusion
            .get(class)
            .and_then(|r| r.get(class))
            .copied()
            .unwrap_or(0)
    }

    /// Samples whose true class is `class` (row sum) — the support.
    fn support(&self, class: usize) -> u64 {
        self.confusion
            .get(class)
            .map(|row| row.iter().fold(0u64, |a, &c| a.saturating_add(c)))
            .unwrap_or(0)
    }

    /// Samples predicted `class` (column sum).
    fn predicted(&self, class: usize) -> u64 {
        (0..self.n_classes).fold(0u64, |a, t| {
            a.saturating_add(
                self.confusion
                    .get(t)
                    .and_then(|r| r.get(class))
                    .copied()
                    .unwrap_or(0),
            )
        })
    }

    /// Recall (‰) for `class`: of its true samples, how many were caught.
    #[must_use]
    pub fn recall_pm(&self, class: usize) -> u64 {
        ratio_pm(self.tp(class), self.support(class))
    }

    /// Precision (‰) for `class`: of samples predicted `class`, how many were right.
    #[must_use]
    pub fn precision_pm(&self, class: usize) -> u64 {
        ratio_pm(self.tp(class), self.predicted(class))
    }

    /// F1 (‰) for `class`: the harmonic mean of precision and recall.
    #[must_use]
    pub fn f1_pm(&self, class: usize) -> u64 {
        let p = self.precision_pm(class);
        let r = self.recall_pm(class);
        // 2pr / (p + r), all in per-mille (the extra PER_MILLE cancels).
        let denom = p.saturating_add(r);
        p.saturating_mul(r)
            .saturating_mul(2)
            .checked_div(denom.max(1))
            .unwrap_or(0)
    }

    /// Macro-F1 (‰): the mean F1 over classes that actually occur (support > 0).
    /// The honest headline for imbalanced data — a model cannot raise it by
    /// ignoring the rare classes.
    #[must_use]
    pub fn macro_f1_pm(&self) -> u64 {
        let mut sum = 0u64;
        let mut present = 0u64;
        for c in 0..self.n_classes {
            if self.support(c) > 0 {
                sum = sum.saturating_add(self.f1_pm(c));
                present = present.saturating_add(1);
            }
        }
        sum.checked_div(present.max(1)).unwrap_or(0)
    }

    /// The number of classes with at least one true sample.
    #[must_use]
    pub fn classes_present(&self) -> usize {
        (0..self.n_classes).filter(|&c| self.support(c) > 0).count()
    }
}

/// `numerator / denominator` in per-mille, saturating and division-by-zero-safe.
fn ratio_pm(numerator: u64, denominator: u64) -> u64 {
    numerator
        .saturating_mul(PER_MILLE)
        .checked_div(denominator.max(1))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::Metrics;

    #[test]
    fn perfect_predictions_score_1000() {
        let mut m = Metrics::new(3);
        for c in 0..3u32 {
            for _ in 0..10 {
                m.record(c, c);
            }
        }
        assert_eq!(m.accuracy_pm(), 1000);
        assert_eq!(m.macro_f1_pm(), 1000);
    }

    #[test]
    fn macro_f1_punishes_ignoring_a_rare_class() {
        // 100 of class 0 all correct; 4 of class 1 all mispredicted as 0.
        let mut m = Metrics::new(2);
        for _ in 0..100 {
            m.record(0, 0);
        }
        for _ in 0..4 {
            m.record(1, 0);
        }
        // Accuracy looks high (100/104 ≈ 962‰) but macro-F1 is dragged down by
        // class 1 having recall 0 (F1 0), so macro-F1 < accuracy.
        assert!(m.accuracy_pm() > 900);
        assert!(
            m.macro_f1_pm() < m.accuracy_pm(),
            "macro-F1 {} must expose the ignored rare class vs accuracy {}",
            m.macro_f1_pm(),
            m.accuracy_pm()
        );
        assert_eq!(m.recall_pm(1), 0);
    }

    #[test]
    fn half_right_is_around_500() {
        let mut m = Metrics::new(2);
        for _ in 0..50 {
            m.record(0, 0);
        }
        for _ in 0..50 {
            m.record(0, 1);
        }
        assert_eq!(m.accuracy_pm(), 500);
    }
}
