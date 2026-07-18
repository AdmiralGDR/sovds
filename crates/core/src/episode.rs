//! Fault episodes — the dataset's real unit of analysis and training.
//!
//! A record is one cycle; but a *fault* is not a row, it is an **episode**: a
//! contiguous stretch where the system was in one failure class, from onset to
//! recovery. "How ready is the dataset to train?" is answered in independent
//! episodes per class, not rows — a single 40-minute outage is 40 correlated rows
//! but *one* episode, and a model needs many independent episodes, not many rows.
//!
//! This module segments a record stream into episodes with a pure, streaming state
//! machine: feed records in order, and each closed episode is emitted as it ends.
//! It is `no_std`, allocation-free, and holds exactly one open episode at a time
//! (the principle/III: bounded state, no hidden buffers) — the caller collects the
//! emitted episodes at the edge, exactly as the dataset loader collects records.
//!
//! Segmentation is deliberately conservative about continuity. An episode never
//! spans a **collector run boundary** (`tick == 0`, a restart = a discontinuity in
//! observation), so live and back-filled runs — which interleave in file order but
//! each restart `tick` at 0 — are segmented independently and never merged. Within
//! a run, a brief healthy flicker (up to `gap_tolerance` cycles) is tolerated and
//! folded into the same episode, because a fault that clears for one cycle and
//! returns is one incident, not two (mirrors the supervisor's own hysteresis).

use crate::domain::Tick;
use crate::failure::FailureClass;
use crate::fsm::Action;
use crate::record::{Record, Source};

/// Why an episode ended — carries the analytical meaning of its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeEnd {
    /// Cleared: healthy for longer than the flicker tolerance.
    Recovered,
    /// Replaced by a different fault class in the same run (causal transition).
    Transitioned,
    /// Cut by a collector restart (`tick == 0`) — continuity could not be assumed.
    RunRestart,
    /// Still ongoing at the end of the corpus (right-censored, not yet recovered).
    Censored,
}

/// One fault episode: a contiguous run in a single failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episode {
    /// The fault class (never [`FailureClass::None`]).
    pub class: FailureClass,
    /// Logical tick of the first fault cycle (within its run).
    pub start_tick: Tick,
    /// Wall-clock (Unix secs) of onset and of the last fault cycle; `0` if the
    /// records carried no timestamp.
    pub start_wall: u64,
    pub end_wall: u64,
    /// Number of fault cycles in the episode (excludes tolerated healthy flickers).
    pub records: u32,
    /// Healthy cycles absorbed as flickers inside the episode.
    pub gaps: u32,
    /// Non-`Observe` actions the supervisor took (or, in dry-run, would have) during
    /// the episode.
    pub actions: u32,
    /// The most disruptive action reached during the episode.
    pub peak_action: Action,
    /// How the episode ended.
    pub end: EpisodeEnd,
    /// Provenance of the episode (the source of its first cycle).
    pub source: Source,
}

impl Episode {
    /// Wall-clock duration in seconds (`0` if either stamp is unknown).
    #[must_use]
    pub fn duration_secs(&self) -> u64 {
        if self.start_wall == 0 || self.end_wall == 0 {
            return 0;
        }
        self.end_wall.saturating_sub(self.start_wall)
    }
}

/// Disruptiveness rank of an action (higher = more disruptive), matching the
/// escalation ladder's order. Used to track an episode's peak action.
const fn action_rank(a: Action) -> u8 {
    match a {
        Action::Observe => 0,
        Action::SwitchNode => 1,
        Action::RestartSingbox => 2,
        Action::BounceWan => 3,
        Action::FailoverBackup => 4,
        Action::Reboot => 5,
    }
}

/// Tuning for segmentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegConfig {
    /// Consecutive healthy cycles tolerated inside an episode before it is declared
    /// recovered. Aligns with the supervisor's `confirm_threshold` (a fault that
    /// clears for fewer cycles than it takes to confirm a new one is one incident).
    pub gap_tolerance: u32,
}

impl SegConfig {
    /// The default tolerance (5 cycles), matching the default `confirm_threshold`.
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
#[derive(Debug, Clone, Copy)]
struct Open {
    class: FailureClass,
    start_tick: Tick,
    start_wall: u64,
    end_wall: u64,
    records: u32,
    gaps: u32,
    pending_gap: u32,
    actions: u32,
    peak_action: Action,
    source: Source,
}

/// A streaming segmenter: feed records in chronological (file) order.
#[derive(Debug, Clone, Copy)]
pub struct Segmenter {
    cfg: SegConfig,
    open: Option<Open>,
}

impl Segmenter {
    /// A fresh segmenter.
    #[must_use]
    pub const fn new(cfg: SegConfig) -> Self {
        Self { cfg, open: None }
    }

    /// Fold one record in. Returns an [`Episode`] at the instant one closes (a
    /// recovery, a transition to another fault, or a run restart); otherwise `None`.
    pub fn observe(&mut self, r: &Record) -> Option<Episode> {
        // A collector restart is a hard discontinuity: close whatever was open
        // before interpreting this record as the start of a new run.
        let mut emitted = None;
        if r.tick.0 == 0 {
            emitted = self.close(EpisodeEnd::RunRestart);
        }

        let class = r.failure;
        match self.open {
            Some(ref mut op) if op.class == class => {
                // Same fault continues: fold in, healing any pending flicker.
                op.gaps = op.gaps.saturating_add(op.pending_gap);
                op.pending_gap = 0;
                op.records = op.records.saturating_add(1);
                if r.wall_time > 0 {
                    op.end_wall = r.wall_time;
                }
                if !matches!(r.action, Action::Observe) {
                    op.actions = op.actions.saturating_add(1);
                    if action_rank(r.action) > action_rank(op.peak_action) {
                        op.peak_action = r.action;
                    }
                }
                emitted
            }
            Some(ref mut op) if class == FailureClass::None => {
                // A healthy cycle: tolerate a brief flicker, else declare recovery.
                op.pending_gap = op.pending_gap.saturating_add(1);
                if op.pending_gap > self.cfg.gap_tolerance {
                    // `emitted` is necessarily None here (a run restart would have
                    // cleared `open`), so returning the recovery cannot drop one.
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
                if class != FailureClass::None {
                    self.begin(r, class);
                }
                emitted
            }
        }
    }

    /// Emit any still-open episode at the end of the stream (right-censored).
    pub fn finish(&mut self) -> Option<Episode> {
        self.close(EpisodeEnd::Censored)
    }

    /// Open a new episode from a fault record.
    fn begin(&mut self, r: &Record, class: FailureClass) {
        let action_is_real = !matches!(r.action, Action::Observe);
        self.open = Some(Open {
            class,
            start_tick: r.tick,
            start_wall: r.wall_time,
            end_wall: r.wall_time,
            records: 1,
            gaps: 0,
            pending_gap: 0,
            actions: u32::from(action_is_real),
            peak_action: r.action,
            source: r.source,
        });
    }

    /// Close the open episode (if any) with `end`, returning it.
    fn close(&mut self, end: EpisodeEnd) -> Option<Episode> {
        let op = self.open.take()?;
        Some(Episode {
            class: op.class,
            start_tick: op.start_tick,
            start_wall: op.start_wall,
            end_wall: op.end_wall,
            records: op.records,
            gaps: op.gaps,
            actions: op.actions,
            peak_action: op.peak_action,
            end,
            source: op.source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{EpisodeEnd, SegConfig, Segmenter};
    use crate::domain::{Aux, Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use crate::failure::FailureClass;
    use crate::fleet::NodeVerdict;
    use crate::fsm::{Action, Reason, State};
    use crate::record::{Outcome, Record, Source};

    /// A record at `tick` classified to `class`, with `action`.
    fn rec(tick: u64, wall: u64, class: FailureClass, action: Action) -> Record {
        Record {
            tick: Tick(tick),
            wall_time: wall,
            probed: true,
            telemetry: Telemetry {
                wan: Wan::Up { uptime_s: 1 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(50),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
                aux: Aux::UNKNOWN,
            },
            path_verdict: NodeVerdict::Healthy,
            failure: class,
            state: State::Healthy,
            reason: Reason::Nominal,
            action,
            outcome: Outcome::NotActed,
            source: Source::Live,
        }
    }

    /// Drive the segmenter over a stream and collect all episodes.
    fn segment(records: &[Record], cfg: SegConfig) -> std::vec::Vec<super::Episode> {
        let mut seg = Segmenter::new(cfg);
        let mut out = std::vec::Vec::new();
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
        let recs: std::vec::Vec<_> = (0..10)
            .map(|t| rec(t, 1000 + t, FailureClass::None, Action::Observe))
            .collect();
        assert!(segment(&recs, SegConfig::new()).is_empty());
    }

    #[test]
    fn one_contiguous_fault_is_one_episode_with_duration() {
        let mut recs = std::vec::Vec::new();
        recs.push(rec(0, 1000, FailureClass::None, Action::Observe));
        for t in 1..=6 {
            recs.push(rec(
                t,
                1000 + t * 60,
                FailureClass::NodeDegraded,
                Action::Observe,
            ));
        }
        recs.push(rec(
            7,
            1000 + 7 * 60,
            FailureClass::NodeDegraded,
            Action::SwitchNode,
        ));
        // recovery
        for t in 8..=20 {
            recs.push(rec(t, 1000 + t * 60, FailureClass::None, Action::Observe));
        }
        let eps = segment(&recs, SegConfig::new());
        assert_eq!(eps.len(), 1);
        let ok = eps.iter().any(|e| {
            e.class == FailureClass::NodeDegraded
                && e.records == 7
                && e.end == EpisodeEnd::Recovered
                && e.actions == 1
                && e.peak_action == Action::SwitchNode
                // onset tick1 .. last fault tick7
                && e.duration_secs() == 7 * 60 - 60
        });
        assert!(ok, "unexpected episode: {:?}", eps.first());
    }

    #[test]
    fn a_short_flicker_does_not_split_one_episode() {
        // fault, one healthy blip (< tolerance), fault again = ONE episode.
        let recs = [
            rec(0, 1000, FailureClass::NodeDead, Action::Observe),
            rec(1, 1060, FailureClass::NodeDead, Action::Observe),
            rec(2, 1120, FailureClass::None, Action::Observe), // 1-cycle flicker
            rec(3, 1180, FailureClass::NodeDead, Action::Observe),
            rec(4, 1240, FailureClass::None, Action::Observe),
            rec(5, 1300, FailureClass::None, Action::Observe),
            rec(6, 1360, FailureClass::None, Action::Observe),
            rec(7, 1420, FailureClass::None, Action::Observe),
            rec(8, 1480, FailureClass::None, Action::Observe),
            rec(9, 1540, FailureClass::None, Action::Observe), // > tolerance -> recovered
        ];
        let eps = segment(&recs, SegConfig::new());
        assert_eq!(eps.len(), 1);
        let ok = eps
            .iter()
            .any(|e| e.records == 3 && e.gaps == 1 && e.end == EpisodeEnd::Recovered);
        assert!(ok, "the tolerated flicker should stay one episode: {eps:?}");
    }

    #[test]
    fn a_different_fault_transitions_into_a_new_episode() {
        let recs = [
            rec(0, 1000, FailureClass::NodeDegraded, Action::Observe),
            rec(1, 1060, FailureClass::NodeDegraded, Action::Observe),
            rec(2, 1120, FailureClass::NodeDead, Action::Observe),
            rec(3, 1180, FailureClass::NodeDead, Action::Observe),
        ];
        let eps = segment(&recs, SegConfig::new());
        assert_eq!(eps.len(), 2);
        let mut it = eps.iter();
        assert!(matches!(it.next(),
            Some(e) if e.class == FailureClass::NodeDegraded && e.end == EpisodeEnd::Transitioned));
        assert!(matches!(it.next(),
            Some(e) if e.class == FailureClass::NodeDead && e.end == EpisodeEnd::Censored));
    }

    #[test]
    fn a_run_restart_closes_an_open_episode() {
        let recs = [
            rec(5, 1000, FailureClass::WanDown, Action::Observe),
            rec(6, 1060, FailureClass::WanDown, Action::Observe),
            // collector restart: tick resets to 0 mid-fault.
            rec(0, 2000, FailureClass::WanDown, Action::Observe),
            rec(1, 2060, FailureClass::WanDown, Action::Observe),
        ];
        let eps = segment(&recs, SegConfig::new());
        assert_eq!(eps.len(), 2, "the restart splits one apparent fault in two");
        let mut it = eps.iter();
        assert!(matches!(it.next(), Some(e) if e.end == EpisodeEnd::RunRestart));
        assert!(matches!(it.next(), Some(e) if e.class == FailureClass::WanDown));
    }
}
