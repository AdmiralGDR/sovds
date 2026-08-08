// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fault episodes — the dataset's real unit of analysis and training.
//!
//! A record is one cycle; but a *fault* is not a row, it is an **episode**: a
//! contiguous stretch in one fault class, from onset to recovery. "How ready is
//! the dataset?" is answered in independent episodes per class, not rows — a single
//! long outage is many correlated rows but *one* episode.
//!
//! This module segments a [`Record`] stream into episodes with a pure, streaming
//! state machine: feed records in order, each closed episode is emitted as it ends.
//! Generic over the caller's [`Domain`], `no_std`, allocation-free, holding exactly
//! one open episode at a time (bounded state, no hidden buffers).
//!
//! Segmentation is conservative about continuity: an episode never spans a
//! collection-run boundary (`tick == 0`, a restart = a discontinuity), and a brief
//! healthy flicker (up to `gap_tolerance` cycles) is folded into the same episode,
//! because a fault that clears for one cycle and returns is one incident.

use crate::domain::Domain;
use crate::record::{Record, Source, Tick};

/// Why an episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeEnd {
    /// Cleared: healthy for longer than the flicker tolerance.
    Recovered,
    /// Replaced by a different fault class in the same run (causal transition).
    Transitioned,
    /// Cut by a collection restart (`tick == 0`).
    RunRestart,
    /// Still ongoing at the end of the corpus (right-censored).
    Censored,
}

/// One fault episode: a contiguous run in a single fault class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episode<D: Domain> {
    /// The fault class (never [`Domain::healthy`]).
    pub class: D::Fault,
    /// Logical tick of the first fault cycle (within its run).
    pub start_tick: Tick,
    /// Wall-clock (Unix secs) of onset; `0` if unknown.
    pub start_wall: u64,
    /// Wall-clock (Unix secs) of the last fault cycle; `0` if unknown.
    pub end_wall: u64,
    /// Fault cycles in the episode (excludes tolerated healthy flickers).
    pub records: u32,
    /// Healthy cycles absorbed as flickers inside the episode.
    pub gaps: u32,
    /// How the episode ended.
    pub end: EpisodeEnd,
    /// Provenance of the episode (source of its first cycle).
    pub source: Source,
    /// Ground-truth injected class if any cycle in the episode was labelled a
    /// deliberate injection — distinguishes an episode we induced from a real one.
    pub injected: Option<D::Fault>,
}

impl<D: Domain> Episode<D> {
    /// Wall-clock duration in seconds (`0` if either stamp is unknown).
    #[must_use]
    pub fn duration_secs(&self) -> u64 {
        if self.start_wall == 0 || self.end_wall == 0 {
            return 0;
        }
        self.end_wall.saturating_sub(self.start_wall)
    }
}

/// Tuning for segmentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegConfig {
    /// Consecutive healthy cycles tolerated inside an episode before it is declared
    /// recovered. A fault that clears for fewer cycles is one incident.
    pub gap_tolerance: u32,
}

impl SegConfig {
    /// The default tolerance (5 cycles).
    #[must_use]
    pub const fn new() -> Self {
        Self { gap_tolerance: 5 }
    }
}

impl Default for SegConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The open episode being accumulated.
#[derive(Clone, Copy)]
struct Open<D: Domain> {
    class: D::Fault,
    start_tick: Tick,
    start_wall: u64,
    end_wall: u64,
    records: u32,
    gaps: u32,
    pending_gap: u32,
    source: Source,
    injected: Option<D::Fault>,
}

/// Drive a fresh [`Segmenter`] over a whole chronological record slice and collect
/// every episode (a convenience over the streaming API; needs `std` for the `Vec`).
#[cfg(feature = "std")]
#[must_use]
pub fn segment<D: Domain>(records: &[Record<D>], cfg: SegConfig) -> Vec<Episode<D>> {
    let mut seg = Segmenter::new(cfg);
    let mut out = Vec::new();
    for r in records {
        if let Some(ep) = seg.observe(r) {
            out.push(ep);
        }
    }
    if let Some(ep) = seg.finish() {
        out.push(ep);
    }
    out
}

/// A streaming segmenter: feed records in chronological (file) order.
#[derive(Clone, Copy)]
pub struct Segmenter<D: Domain> {
    cfg: SegConfig,
    open: Option<Open<D>>,
}

impl<D: Domain> Segmenter<D> {
    /// A fresh segmenter.
    #[must_use]
    pub const fn new(cfg: SegConfig) -> Self {
        Self { cfg, open: None }
    }

    /// Fold one record in. Returns an [`Episode`] at the instant one closes (a
    /// recovery, a transition, or a run restart); otherwise `None`.
    pub fn observe(&mut self, r: &Record<D>) -> Option<Episode<D>> {
        // A collection restart is a hard discontinuity: close whatever was open
        // before interpreting this record as the start of a new run.
        let mut emitted = None;
        if r.tick.0 == 0 {
            emitted = self.close(EpisodeEnd::RunRestart);
        }

        let class = r.class;
        let healthy = D::healthy();
        match self.open {
            Some(ref mut op) if op.class == class => {
                // Same fault continues: fold in, healing any pending flicker.
                op.gaps = op.gaps.saturating_add(op.pending_gap);
                op.pending_gap = 0;
                op.records = op.records.saturating_add(1);
                if r.wall_time > 0 {
                    op.end_wall = r.wall_time;
                }
                if op.injected.is_none() {
                    op.injected = r.label;
                }
                emitted
            }
            Some(ref mut op) if class == healthy => {
                // A healthy cycle: tolerate a brief flicker, else declare recovery.
                op.pending_gap = op.pending_gap.saturating_add(1);
                if op.pending_gap > self.cfg.gap_tolerance {
                    // `emitted` is necessarily None here (a run restart would have
                    // cleared `open`), so returning recovery cannot drop one.
                    self.close(EpisodeEnd::Recovered)
                } else {
                    emitted
                }
            }
            Some(_) => {
                // A different fault: the old one transitions into the new.
                let closed = self.close(EpisodeEnd::Transitioned);
                self.begin(r, class);
                closed
            }
            None => {
                if class != healthy {
                    self.begin(r, class);
                }
                emitted
            }
        }
    }

    /// Emit any still-open episode at the end of the stream (right-censored).
    pub fn finish(&mut self) -> Option<Episode<D>> {
        self.close(EpisodeEnd::Censored)
    }

    /// Open a new episode from a fault record.
    fn begin(&mut self, r: &Record<D>, class: D::Fault) {
        self.open = Some(Open {
            class,
            start_tick: r.tick,
            start_wall: r.wall_time,
            end_wall: r.wall_time,
            records: 1,
            gaps: 0,
            pending_gap: 0,
            source: r.source,
            injected: r.label,
        });
    }

    /// Close the open episode (if any) with `end`, returning it.
    fn close(&mut self, end: EpisodeEnd) -> Option<Episode<D>> {
        let op = self.open.take()?;
        Some(Episode {
            class: op.class,
            start_tick: op.start_tick,
            start_wall: op.start_wall,
            end_wall: op.end_wall,
            records: op.records,
            gaps: op.gaps,
            end,
            source: op.source,
            injected: op.injected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Episode, EpisodeEnd, SegConfig, Segmenter};
    use crate::testkit::{rec, Health, Svc};

    fn segment(records: &[crate::record::Record<Svc>]) -> Vec<Episode<Svc>> {
        let mut seg = Segmenter::new(SegConfig::new());
        let mut out = Vec::new();
        for r in records {
            if let Some(ep) = seg.observe(r) {
                out.push(ep);
            }
        }
        if let Some(ep) = seg.finish() {
            out.push(ep);
        }
        out
    }

    #[test]
    fn a_healthy_only_stream_has_no_episodes() {
        let recs: Vec<_> = (0..10).map(|t| rec(t, 1000 + t, Health::Ok)).collect();
        assert!(segment(&recs).is_empty());
    }

    #[test]
    fn one_contiguous_fault_is_one_episode_with_duration() {
        let mut recs = vec![rec(0, 1000, Health::Ok)];
        for t in 1..=6 {
            recs.push(rec(t, 1000 + t * 60, Health::Overloaded));
        }
        for t in 7..=20 {
            recs.push(rec(t, 1000 + t * 60, Health::Ok));
        }
        let eps = segment(&recs);
        assert_eq!(eps.len(), 1);
        let ok = eps.iter().any(|e| {
            e.class == Health::Overloaded
                && e.records == 6
                && e.end == EpisodeEnd::Recovered
                && e.duration_secs() == 6 * 60 - 60
        });
        assert!(ok, "unexpected: {:?}", eps.first());
    }

    #[test]
    fn a_short_flicker_does_not_split_one_episode() {
        let recs = [
            rec(0, 1000, Health::Unreachable),
            rec(1, 1060, Health::Unreachable),
            rec(2, 1120, Health::Ok), // 1-cycle flicker
            rec(3, 1180, Health::Unreachable),
            rec(4, 1240, Health::Ok),
            rec(5, 1300, Health::Ok),
            rec(6, 1360, Health::Ok),
            rec(7, 1420, Health::Ok),
            rec(8, 1480, Health::Ok),
            rec(9, 1540, Health::Ok), // > tolerance -> recovered
        ];
        let eps = segment(&recs);
        assert_eq!(eps.len(), 1);
        let ok = eps
            .iter()
            .any(|e| e.records == 3 && e.gaps == 1 && e.end == EpisodeEnd::Recovered);
        assert!(ok, "flicker should stay one episode: {eps:?}");
    }

    #[test]
    fn a_different_fault_transitions_into_a_new_episode() {
        let recs = [
            rec(0, 1000, Health::Overloaded),
            rec(1, 1060, Health::Overloaded),
            rec(2, 1120, Health::Unreachable),
            rec(3, 1180, Health::Unreachable),
        ];
        let eps = segment(&recs);
        assert_eq!(eps.len(), 2);
        let mut it = eps.iter();
        assert!(matches!(it.next(),
            Some(e) if e.class == Health::Overloaded && e.end == EpisodeEnd::Transitioned));
        assert!(matches!(it.next(),
            Some(e) if e.class == Health::Unreachable && e.end == EpisodeEnd::Censored));
    }

    #[test]
    fn a_run_restart_closes_an_open_episode() {
        let recs = [
            rec(5, 1000, Health::Unreachable),
            rec(6, 1060, Health::Unreachable),
            // restart: tick resets to 0 mid-fault.
            rec(0, 2000, Health::Unreachable),
            rec(1, 2060, Health::Unreachable),
        ];
        let eps = segment(&recs);
        assert_eq!(eps.len(), 2, "the restart splits one apparent fault in two");
        let mut it = eps.iter();
        assert!(matches!(it.next(), Some(e) if e.end == EpisodeEnd::RunRestart));
        assert!(matches!(it.next(), Some(e) if e.class == Health::Unreachable));
    }

    #[test]
    fn an_injected_label_marks_the_episode() {
        let mut a = rec(0, 1000, Health::Overloaded);
        a.label = Some(Health::Overloaded);
        let recs = [a, rec(1, 1060, Health::Overloaded)];
        let eps = segment(&recs);
        assert_eq!(eps.len(), 1);
        assert!(matches!(eps.first(), Some(e) if e.injected == Some(Health::Overloaded)));
    }
}
