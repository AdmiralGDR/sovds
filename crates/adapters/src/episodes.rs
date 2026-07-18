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

/// Detection scorecard for deliberately injected faults: over records carrying a
/// ground-truth `inject=` label, how often did the rules diagnose the class we
/// actually induced? This is the payoff of labelled injection — it measures the
/// classifier on real router behaviour, not synthetic telemetry (the principle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectionStat {
    pub class: FailureClass,
    /// Records taken while this class was being injected.
    pub labeled: usize,
    /// Of those, records the rules classified as the injected class (a hit).
    pub detected: usize,
}

/// Score injection detection per class, in canonical order, skipping classes that
/// were never injected.
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
        let labeled = records.iter().filter(|r| r.label == Some(class)).count();
        if labeled == 0 {
            return None;
        }
        let detected = records
            .iter()
            .filter(|r| r.label == Some(class) && r.failure == class)
            .count();
        Some(InjectionStat {
            class,
            labeled,
            detected,
        })
    })
    .collect()
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
    fn injection_accuracy_scores_hits_and_misses() {
        use super::injection_accuracy;
        // Two records injected as singbox_dead: one correctly diagnosed, one missed
        // (e.g. a dry-run window where nothing actually broke -> class=none).
        let mut hit = rec(1, 100, FailureClass::SingboxDead);
        hit.label = Some(FailureClass::SingboxDead);
        let mut miss = rec(2, 160, FailureClass::None);
        miss.label = Some(FailureClass::SingboxDead);
        let organic = rec(3, 220, FailureClass::NodeDegraded); // no label
        let stats = injection_accuracy(&[hit, miss, organic]);
        assert_eq!(stats.len(), 1, "only injected classes are scored");
        let s = stats.iter().find(|s| s.class == FailureClass::SingboxDead);
        assert!(matches!(s, Some(st) if st.labeled == 2 && st.detected == 1));
    }
}
