// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Training-readiness: roll fault episodes up per class into the numbers that
//! gate "is there enough data yet?" — measured in **independent episodes per
//! class**, not rows (a healthy system fails rarely, so the rare classes
//! converge in episodes agonisingly slowly; this says how slowly).
//!
//! Offline analysis, so it links `std` (behind the crate's `std` feature) and uses
//! `Vec`/sort freely — the no-alloc discipline binds the on-device path, not the
//! dataset tooling.

use crate::domain::Domain;
use crate::episode::{Episode, EpisodeEnd};

/// Per-class rollup of episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassReadiness<D: Domain> {
    /// The fault class this row summarises.
    pub class: D::Fault,
    /// Independent episodes of this class.
    pub count: usize,
    /// How many ended cleanly by recovery.
    pub recovered: usize,
    /// Total fault cycles across all episodes of this class.
    pub total_records: u64,
    /// Total wall-clock seconds across episodes with known timestamps.
    pub total_secs: u64,
    /// Median episode duration in seconds (0 if no timed episodes).
    pub median_secs: u64,
}

/// Roll episodes up per fault class, in the domain's canonical fault order
/// (excluding [`Domain::healthy`]).
#[must_use]
pub fn summarize<D: Domain>(episodes: &[Episode<D>]) -> Vec<ClassReadiness<D>> {
    let healthy = D::healthy();
    D::faults()
        .iter()
        .filter(|&&c| c != healthy)
        .map(|&class| rollup(class, episodes))
        .collect()
}

fn rollup<D: Domain>(class: D::Fault, episodes: &[Episode<D>]) -> ClassReadiness<D> {
    let mut count = 0usize;
    let mut recovered = 0usize;
    let mut total_records = 0u64;
    let mut total_secs = 0u64;
    let mut durations: Vec<u64> = Vec::new();

    for ep in episodes.iter().filter(|e| e.class == class) {
        count = count.saturating_add(1);
        if matches!(ep.end, EpisodeEnd::Recovered) {
            recovered = recovered.saturating_add(1);
        }
        total_records = total_records.saturating_add(u64::from(ep.records));
        let secs = ep.duration_secs();
        total_secs = total_secs.saturating_add(secs);
        if secs > 0 {
            durations.push(secs);
        }
    }

    durations.sort_unstable();
    let median_secs = durations
        .len()
        .checked_div(2)
        .and_then(|mid| durations.get(mid))
        .copied()
        .unwrap_or(0);

    ClassReadiness {
        class,
        count,
        recovered,
        total_records,
        total_secs,
        median_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use crate::episode::{SegConfig, Segmenter};
    use crate::testkit::{rec, Health};

    #[test]
    fn counts_independent_episodes_per_class() {
        // Two separate Overloaded episodes (split by a long healthy stretch) + one
        // Unreachable episode.
        let mut recs = vec![rec(0, 100, Health::Ok)];
        recs.push(rec(1, 160, Health::Overloaded));
        recs.push(rec(2, 220, Health::Overloaded));
        for t in 3..12 {
            recs.push(rec(t, 100 + t * 60, Health::Ok));
        }
        recs.push(rec(12, 100 + 12 * 60, Health::Overloaded));
        for t in 13..22 {
            recs.push(rec(t, 100 + t * 60, Health::Ok));
        }
        recs.push(rec(22, 100 + 22 * 60, Health::Unreachable));
        for t in 23..30 {
            recs.push(rec(t, 100 + t * 60, Health::Ok));
        }

        let mut seg = Segmenter::new(SegConfig::new());
        let mut eps = Vec::new();
        for r in &recs {
            if let Some(e) = seg.observe(r) {
                eps.push(e);
            }
        }
        if let Some(e) = seg.finish() {
            eps.push(e);
        }

        let summary = summarize(&eps);
        let ov = summary.iter().find(|c| c.class == Health::Overloaded);
        assert!(matches!(ov, Some(c) if c.count == 2 && c.recovered == 2 && c.total_records == 3));
    }
}
