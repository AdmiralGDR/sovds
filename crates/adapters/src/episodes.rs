//! Episode view of a corpus: segment records into fault episodes and summarise
//! the dataset's training-readiness per class.
//!
//! The [`sovereign_core::episode`] segmenter is the pure logic; this is the
//! offline (`std`) edge that drives it over a loaded corpus and rolls the emitted
//! episodes up into the numbers that actually gate "can we train yet?" — namely
//! **independent episodes per fault class**, not raw row counts (the principle: measure
//! the right thing). A healthy system rarely fails, so the rare classes converge
//! in episodes agonisingly slowly; this is the instrument that says how slowly.

use sovereign_core::episode::{Episode, SegConfig, Segmenter};
use sovereign_core::failure::FailureClass;
use sovereign_core::record::Record;

/// Segment a chronological record slice into fault episodes.
#[must_use]
pub fn segment(records: &[Record], cfg: SegConfig) -> Vec<Episode> {
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

/// Per-class rollup of episodes — the training-readiness picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassEpisodes {
    pub class: FailureClass,
    /// Independent episodes of this class.
    pub count: usize,
    /// How many ended cleanly by recovery (vs transition/restart/censored).
    pub recovered: usize,
    /// How many involved at least one non-`Observe` remedy.
    pub with_action: usize,
    /// Total fault cycles across all episodes of this class.
    pub total_records: u64,
    /// Total wall-clock seconds across episodes with known timestamps.
    pub total_secs: u64,
    /// Median episode duration in seconds (0 if no timed episodes).
    pub median_secs: u64,
}

/// Roll episodes up per fault class, in the canonical class order (excluding the
/// non-fault `None`).
#[must_use]
pub fn summarize(episodes: &[Episode]) -> Vec<ClassEpisodes> {
    [
        FailureClass::WanDown,
        FailureClass::SingboxDead,
        FailureClass::NodeDead,
        FailureClass::NodeDegraded,
        FailureClass::DnsFail,
    ]
    .into_iter()
    .map(|class| rollup(class, episodes))
    .collect()
}

/// Rollback-safety tally for an autonomous fault injector: for each *injection
/// window* (a contiguous run of `inject=`-labelled records), did the system return
/// to health afterwards? A `stuck` count above zero means an injection did not
/// revert — the one thing an unattended disruptor must never do (the principle/III:
/// bounded, self-heal).
///
/// This is measured over records, at the **window** level, not per fault episode —
/// so a class transition inside one window (e.g. `dns_fail` → `singbox_dead` as a
/// tunnel drops) is one injection, not two, and is not mistaken for a failed
/// rollback. Right-censoring is honoured: a window at the very tail of the corpus,
/// whose recovery simply has not been observed yet, is `pending`, never `stuck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InjectionSafety {
    /// Distinct injection windows seen.
    pub windows: usize,
    /// Windows followed by a healthy (`class=none`) record — a confirmed recovery.
    pub recovered: usize,
    /// Windows followed only by faulty records — a rollback that did NOT take.
    pub stuck: usize,
    /// Windows at the corpus tail with no following record yet — recovery not yet
    /// observable (not a failure).
    pub pending: usize,
}

/// Tally injection rollback safety over a chronological record slice.
#[must_use]
pub fn injection_safety(records: &[Record]) -> InjectionSafety {
    let mut s = InjectionSafety::default();
    let mut i = 0usize;
    while let Some(rec) = records.get(i) {
        if rec.label.is_none() {
            i = i.saturating_add(1);
            continue;
        }
        // Advance to the end of this contiguous injection window.
        let mut j = i;
        while records.get(j).is_some_and(|r| r.label.is_some()) {
            j = j.saturating_add(1);
        }
        s.windows = s.windows.saturating_add(1);
        let tail = records.get(j..).unwrap_or(&[]);
        if tail.is_empty() {
            // Window is right-censored: recovery cannot be observed yet.
            s.pending = s.pending.saturating_add(1);
        } else if tail.iter().any(|r| r.failure == FailureClass::None) {
            // The system was seen healthy again after the window: recovered.
            s.recovered = s.recovered.saturating_add(1);
        } else {
            // Records follow but the system never returned to health: stuck.
            s.stuck = s.stuck.saturating_add(1);
        }
        i = j;
    }
    s
}

/// Detection scorecard for deliberately injected faults, measured **per injection
/// window** — the honest question is "of the times we induced this fault, in how
/// many did the brain diagnose it?", not "what fraction of labelled rows matched"
/// (the principle: measure the right thing).
///
/// A per-record rate is misleading: one injection window spans a transition edge
/// (e.g. DNS fails for a cycle as the tunnel drops during a `singbox_dead` break)
/// and possibly a dry-run marker where nothing actually broke. Scoring per record
/// counts those as misses though the brain behaved correctly. Per window, each
/// window falls into exactly one bucket:
/// * **detected** — at least one cycle in the window classified as the injected
///   class (the brain caught it);
/// * **missed** — the window showed a fault (some non-`none` class) but never the
///   injected one (diagnosed the wrong fault);
/// * **silent** — every cycle read `none`: the fault never manifested in telemetry
///   (a dry-run marker, or the probe cadence fell entirely outside the real break)
///   — not a classifier failure, reported apart so it can't drag the rate down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectionStat {
    pub class: FailureClass,
    /// Injection windows of this class.
    pub windows: usize,
    /// Windows where the brain diagnosed the injected class at least once.
    pub detected: usize,
    /// Windows that showed a fault, but never the injected class.
    pub missed: usize,
    /// Windows where the fault never manifested in telemetry (dry-run / cadence miss).
    pub silent: usize,
    /// Total labelled records (dataset volume, for context).
    pub records: usize,
}

/// Score injection detection per class, per window, in canonical order, skipping
/// classes that were never injected.
#[must_use]
pub fn injection_accuracy(records: &[Record]) -> Vec<InjectionStat> {
    [
        FailureClass::WanDown,
        FailureClass::SingboxDead,
        FailureClass::NodeDead,
        FailureClass::NodeDegraded,
        FailureClass::DnsFail,
    ]
    .into_iter()
    .filter_map(|class| {
        let stat = window_stat(class, records);
        (stat.windows > 0).then_some(stat)
    })
    .collect()
}

/// Per-window detection tally for one class over the record stream.
fn window_stat(class: FailureClass, records: &[Record]) -> InjectionStat {
    let mut stat = InjectionStat {
        class,
        windows: 0,
        detected: 0,
        missed: 0,
        silent: 0,
        records: 0,
    };
    let mut i = 0usize;
    while let Some(rec) = records.get(i) {
        if rec.label != Some(class) {
            i = i.saturating_add(1);
            continue;
        }
        // Walk to the end of this contiguous same-class injection window.
        let mut j = i;
        let mut hit = false;
        let mut any_fault = false;
        while records.get(j).is_some_and(|r| r.label == Some(class)) {
            if let Some(r) = records.get(j) {
                stat.records = stat.records.saturating_add(1);
                if r.failure == class {
                    hit = true;
                }
                if r.failure != FailureClass::None {
                    any_fault = true;
                }
            }
            j = j.saturating_add(1);
        }
        stat.windows = stat.windows.saturating_add(1);
        if hit {
            stat.detected = stat.detected.saturating_add(1);
        } else if any_fault {
            stat.missed = stat.missed.saturating_add(1);
        } else {
            stat.silent = stat.silent.saturating_add(1);
        }
        i = j;
    }
    stat
}

fn rollup(class: FailureClass, episodes: &[Episode]) -> ClassEpisodes {
    let mut count = 0usize;
    let mut recovered = 0usize;
    let mut with_action = 0usize;
    let mut total_records = 0u64;
    let mut total_secs = 0u64;
    let mut durations: Vec<u64> = Vec::new();

    for ep in episodes.iter().filter(|e| e.class == class) {
        count = count.saturating_add(1);
        if matches!(ep.end, sovereign_core::episode::EpisodeEnd::Recovered) {
            recovered = recovered.saturating_add(1);
        }
        if ep.actions > 0 {
            with_action = with_action.saturating_add(1);
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

    ClassEpisodes {
        class,
        count,
        recovered,
        with_action,
        total_records,
        total_secs,
        median_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::{segment, summarize};
    use sovereign_core::domain::{
        Aux, Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES,
    };
    use sovereign_core::episode::SegConfig;
    use sovereign_core::failure::FailureClass;
    use sovereign_core::fleet::NodeVerdict;
    use sovereign_core::fsm::{Action, Reason, State};
    use sovereign_core::record::{Outcome, Record, Source};

    fn rec(tick: u64, wall: u64, class: FailureClass) -> Record {
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
            action: Action::Observe,
            outcome: Outcome::NotActed,
            source: Source::Live,
            label: None,
        }
    }

    #[test]
    fn summary_counts_independent_episodes_per_class() {
        // Two separate dns_fail episodes (separated by a long healthy stretch) and
        // one node_dead episode.
        let mut recs = Vec::new();
        recs.push(rec(0, 100, FailureClass::None));
        recs.push(rec(1, 160, FailureClass::DnsFail));
        recs.push(rec(2, 220, FailureClass::DnsFail));
        for t in 3..12 {
            recs.push(rec(t, 100 + t * 60, FailureClass::None));
        }
        recs.push(rec(12, 100 + 12 * 60, FailureClass::DnsFail));
        for t in 13..22 {
            recs.push(rec(t, 100 + t * 60, FailureClass::None));
        }
        recs.push(rec(22, 100 + 22 * 60, FailureClass::NodeDead));
        for t in 23..30 {
            recs.push(rec(t, 100 + t * 60, FailureClass::None));
        }

        let eps = segment(&recs, SegConfig::new());
        let summary = summarize(&eps);
        let dns = summary
            .iter()
            .find(|c| c.class == FailureClass::DnsFail)
            .copied()
            .unwrap_or(super::ClassEpisodes {
                class: FailureClass::DnsFail,
                count: 0,
                recovered: 0,
                with_action: 0,
                total_records: 0,
                total_secs: 0,
                median_secs: 0,
            });
        assert_eq!(dns.count, 2, "two independent dns_fail episodes");
        assert_eq!(dns.recovered, 2);
        assert_eq!(dns.total_records, 3);
    }

    #[test]
    fn injection_accuracy_buckets_windows_detected_missed_silent() {
        use super::injection_accuracy;
        let labeled = |mut r: Record, c: FailureClass| {
            r.label = Some(c);
            r
        };
        // Three singbox_dead windows, separated by healthy gaps:
        //  W1: a transition edge (dns_fail) then the real peak (sb=dead) -> DETECTED.
        //  W2: a dry-run marker, telemetry never broke (all none)        -> SILENT.
        //  W3: showed a fault but only ever the wrong class (node_dead)  -> MISSED.
        let sd = FailureClass::SingboxDead;
        let records = vec![
            labeled(rec(1, 100, FailureClass::DnsFail), sd),
            labeled(rec(2, 160, sd), sd),
            rec(3, 220, FailureClass::None),
            labeled(rec(4, 280, FailureClass::None), sd),
            rec(5, 340, FailureClass::None),
            labeled(rec(6, 400, FailureClass::NodeDead), sd),
            rec(7, 460, FailureClass::None),
        ];
        let stats = injection_accuracy(&records);
        let s = stats.iter().find(|s| s.class == sd);
        assert!(matches!(s, Some(st)
            if st.windows == 3 && st.detected == 1 && st.silent == 1 && st.missed == 1 && st.records == 4));
    }

    #[test]
    fn injection_accuracy_scores_each_class_independently() {
        use super::injection_accuracy;
        // A mixed run scored per class, per window — proves the tooling is
        // class-generic (a new injected class needs no code change).
        let labeled = |mut r: Record, c: FailureClass| {
            r.label = Some(c);
            r
        };
        let records = vec![
            labeled(
                rec(1, 100, FailureClass::SingboxDead),
                FailureClass::SingboxDead,
            ),
            rec(2, 160, FailureClass::None),
            labeled(rec(3, 220, FailureClass::DnsFail), FailureClass::DnsFail),
            rec(4, 280, FailureClass::None),
        ];
        let stats = injection_accuracy(&records);
        assert_eq!(stats.len(), 2, "both injected classes appear");
        let sb = stats.iter().find(|s| s.class == FailureClass::SingboxDead);
        let dns = stats.iter().find(|s| s.class == FailureClass::DnsFail);
        assert!(matches!(sb, Some(s) if s.windows == 1 && s.detected == 1));
        assert!(matches!(dns, Some(s) if s.windows == 1 && s.detected == 1));
    }

    #[test]
    fn injection_safety_distinguishes_recovered_stuck_and_pending() {
        use super::injection_safety;
        let labeled = |mut r: Record| {
            r.label = Some(FailureClass::SingboxDead);
            r
        };
        // Window A: a class transition inside one injection (dns_fail -> singbox_dead)
        // then a healthy record => ONE window, recovered (not two, not stuck).
        // Window B: injection followed only by faulty records => stuck.
        // Window C: injection at the very tail => pending (censored, not a failure).
        let records = vec![
            rec(0, 100, FailureClass::None),
            labeled(rec(1, 160, FailureClass::DnsFail)), // A onset (transition inside)
            labeled(rec(2, 220, FailureClass::SingboxDead)), // A peak
            rec(3, 280, FailureClass::None),             // A recovered
            rec(4, 340, FailureClass::None),
            labeled(rec(5, 400, FailureClass::SingboxDead)), // B
            rec(6, 460, FailureClass::NodeDead),             // B: still faulty, never healthy
            labeled(rec(7, 520, FailureClass::SingboxDead)), // C at the tail
        ];
        let s = injection_safety(&records);
        assert_eq!(s.windows, 3, "three distinct injection windows");
        assert_eq!(s.recovered, 1, "the transition window recovered as one");
        assert_eq!(s.stuck, 1, "the never-healthy window is stuck");
        assert_eq!(s.pending, 1, "the tail window is censored, not stuck");
    }
}
