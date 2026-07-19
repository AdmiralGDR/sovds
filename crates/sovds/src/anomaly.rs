//! Unsupervised anomaly scoring over a record stream — find what the rules missed.
//!
//! The rules classify *known* faults; a healthy-looking record (`class = healthy`)
//! that is nonetheless statistically far from normal is either a novel fault the
//! rule vocabulary does not cover, or a benign oddity. This module surfaces those
//! for human review — it never actuates (the principle: it is an analysis lens, not a
//! decision).
//!
//! It learns, online, a baseline for each of the observation's raw [signals]
//! ([`crate::domain::Domain::signals`]): an EWMA mean and an EWMA of the typical
//! absolute deviation (its spread). A record's **anomaly score** is the sum, over
//! its measured signals, of how many spreads each value sits from its baseline
//! (integer fixed-point, ×[`crate::stats::SCALE`], so a score of 1000 is "one
//! typical deviation away"). Everything is integer and bounded — no floats, no
//! panics, a fixed per-signal state (the principle/V).
//!
//! [signals]: crate::domain::Domain::signals

use crate::domain::Domain;
use crate::record::Record;
use crate::stats::{Ewma, SCALE};

/// Upper bound on the number of signals scored (bounded state, the principle). Signals
/// a domain exposes beyond this are ignored.
pub const MAX_SIGNALS: usize = 16;

/// Sentinel a domain emits for a signal it could not measure this cycle; such a
/// signal does not update its baseline and does not contribute to the score.
const NOT_MEASURED: i64 = -1;

/// Tuning for the scorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnomalyConfig {
    /// EWMA smoothing denominator for each signal's baseline (larger = slower).
    pub weight: u64,
    /// Samples a signal needs before it is scored (avoids reacting to a cold model).
    pub warmup: u32,
    /// Floor on a signal's typical-deviation estimate, so a rock-steady signal does
    /// not turn a tiny wobble into a huge score.
    pub min_spread: u64,
}

impl AnomalyConfig {
    /// Sensible defaults: a moderately slow baseline, 20-sample warmup, spread
    /// floor of 1 (signals are integers).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            weight: 16,
            warmup: 20,
            min_spread: 1,
        }
    }
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One signal's online baseline.
#[derive(Clone, Copy)]
struct SignalModel {
    mean: Ewma,
    spread: Ewma,
    count: u32,
}

impl SignalModel {
    const fn new(weight: u64) -> Self {
        Self {
            mean: Ewma::new(weight),
            spread: Ewma::new(weight),
            count: 0,
        }
    }

    /// Fold in a measured value; return its deviation-in-spreads (×SCALE) once
    /// warmed, else `None`. Scores against the *current* baseline before updating,
    /// so a value cannot mask its own anomaly.
    fn observe(&mut self, value: u64, warmup: u32, min_spread: u64) -> Option<u64> {
        let score = match self.mean.get() {
            None => None, // first sample only seeds the baseline
            Some(mean) => {
                let dev = value.abs_diff(mean);
                let out = if self.count >= warmup {
                    let sp = self.spread.get().unwrap_or(0).max(min_spread);
                    Some(dev.saturating_mul(SCALE).checked_div(sp).unwrap_or(0))
                } else {
                    None
                };
                self.spread.update(dev);
                out
            }
        };
        self.mean.update(value);
        self.count = self.count.saturating_add(1);
        score
    }
}

/// A streaming anomaly scorer over one domain's records. Feed records in order.
#[derive(Clone, Copy)]
pub struct Scorer {
    models: [SignalModel; MAX_SIGNALS],
    cfg: AnomalyConfig,
}

impl Scorer {
    /// A fresh scorer.
    #[must_use]
    pub const fn new(cfg: AnomalyConfig) -> Self {
        Self {
            models: [SignalModel::new(cfg.weight); MAX_SIGNALS],
            cfg,
        }
    }

    /// Fold one record in and return its anomaly score (0 while any needed signal
    /// is still warming). The score is the summed per-signal deviation-in-spreads.
    pub fn observe<D: Domain>(&mut self, r: &Record<D>) -> u64 {
        // Collect the record's signal values into a bounded buffer.
        let mut buf = [0i64; MAX_SIGNALS];
        let mut n = 0usize;
        D::signals(&r.obs, &mut |v| {
            if let Some(slot) = buf.get_mut(n) {
                *slot = v;
                n = n.saturating_add(1);
            }
        });

        let mut total = 0u64;
        for i in 0..n {
            let (Some(model), Some(&value)) = (self.models.get_mut(i), buf.get(i)) else {
                continue;
            };
            if value == NOT_MEASURED {
                continue; // absence does not train and does not score
            }
            let unsigned = u64::try_from(value).unwrap_or(0);
            if let Some(z) = model.observe(unsigned, self.cfg.warmup, self.cfg.min_spread) {
                total = total.saturating_add(z);
            }
        }
        total
    }
}

/// Score a whole corpus, returning one score per record (parallel to `records`).
/// Deterministic; the only state is the advancing baselines.
#[cfg(feature = "std")]
#[must_use]
pub fn score<D: Domain>(records: &[Record<D>], cfg: AnomalyConfig) -> Vec<u64> {
    let mut scorer = Scorer::new(cfg);
    records.iter().map(|r| scorer.observe(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::{score, AnomalyConfig};
    use crate::record::Record;
    use crate::testkit::{rec, Health, Svc};

    #[test]
    fn a_steady_stream_scores_low_then_a_jump_scores_high() {
        // testkit Svc exposes signals [load, over]; rec() sets obs=load=10.
        // Feed a steady baseline, then one record with a wildly different load.
        let mut records: Vec<Record<Svc>> = Vec::new();
        for t in 0..40 {
            let mut r = rec(t, 100 + t, Health::Ok);
            r.obs = 10; // steady load
            records.push(r);
        }
        let mut spike = rec(40, 100, Health::Ok);
        spike.obs = 900; // huge jump — and it flips the `over` signal too
        records.push(spike);

        let scores = score::<Svc>(&records, AnomalyConfig::new());
        let baseline = scores.get(39).copied().unwrap_or(u64::MAX);
        let anomaly = scores.last().copied().unwrap_or(0);
        assert!(baseline < 500, "a steady record scores low, got {baseline}");
        assert!(
            anomaly > baseline.saturating_mul(10),
            "the jump scores far higher ({anomaly} vs {baseline})"
        );
    }

    #[test]
    fn warmup_suppresses_early_scores() {
        // The very first records cannot be anomalous — nothing to compare to yet.
        let records: Vec<Record<Svc>> = (0..3).map(|t| rec(t, 100 + t, Health::Ok)).collect();
        let scores = score::<Svc>(&records, AnomalyConfig::new());
        assert!(
            scores.iter().all(|&s| s == 0),
            "warmup keeps early scores 0"
        );
    }
}
