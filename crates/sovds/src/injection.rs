//! Metrics over deliberately injected faults — the payoff of ground-truth
//! labelling, generic over the caller's [`Domain`].
//!
//! When a fault is induced on purpose, every record in the window carries an
//! `inject=<class>` label ([`Record::label`]). Two questions matter, and both are
//! measured **per injection window**, not per record (the principle: measure the right
//! thing — a window spans transition edges and possibly a dry-run marker where
//! nothing broke, so per-record scoring is misleading):
//!
//! * **Detection** ([`accuracy`]) — of the times we induced a class, in how many
//!   did the diagnosis catch it? Each window is `detected` / `missed` (a fault, but
//!   the wrong class) / `silent` (never manifested — a dry-run or a cadence miss,
//!   reported apart so it can't drag the rate down).
//! * **Rollback safety** ([`safety`]) — did every induced fault revert? The one
//!   thing an unattended injector must never do is fail to recover.

use crate::domain::Domain;
use crate::record::Record;

/// Per-class, per-window detection tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection<D: Domain> {
    pub class: D::Fault,
    /// Injection windows of this class.
    pub windows: usize,
    /// Windows where the diagnosis matched the injected class at least once.
    pub detected: usize,
    /// Windows that showed a fault, but never the injected class.
    pub missed: usize,
    /// Windows where the fault never manifested (dry-run / cadence miss).
    pub silent: usize,
    /// Total labelled records of this class (dataset volume, for context).
    pub records: usize,
}

/// Score injection detection per class, per window, over a chronological record
/// slice — skipping classes that were never injected.
pub fn accuracy<D: Domain>(records: &[Record<D>]) -> impl Iterator<Item = Detection<D>> + '_ {
    D::faults().iter().filter_map(move |&class| {
        let stat = window_stat(class, records);
        (stat.windows > 0).then_some(stat)
    })
}

fn window_stat<D: Domain>(class: D::Fault, records: &[Record<D>]) -> Detection<D> {
    let healthy = D::healthy();
    let mut stat = Detection {
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
                if r.class == class {
                    hit = true;
                }
                if r.class != healthy {
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

/// Rollback-safety tally over injection windows (any contiguous `inject=`-labelled
/// run). A window `recovered` if a later record is healthy, is `stuck` if records
/// follow but none is healthy, and is `pending` if it sits at the corpus tail
/// (right-censored — recovery not yet observable, never a failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Safety {
    pub windows: usize,
    pub recovered: usize,
    pub stuck: usize,
    pub pending: usize,
}

/// Tally injection rollback safety over a chronological record slice.
#[must_use]
pub fn safety<D: Domain>(records: &[Record<D>]) -> Safety {
    let healthy = D::healthy();
    let mut s = Safety::default();
    let mut i = 0usize;
    while let Some(rec) = records.get(i) {
        if rec.label.is_none() {
            i = i.saturating_add(1);
            continue;
        }
        let mut j = i;
        while records.get(j).is_some_and(|r| r.label.is_some()) {
            j = j.saturating_add(1);
        }
        s.windows = s.windows.saturating_add(1);
        let tail = records.get(j..).unwrap_or(&[]);
        if tail.is_empty() {
            s.pending = s.pending.saturating_add(1);
        } else if tail.iter().any(|r| r.class == healthy) {
            s.recovered = s.recovered.saturating_add(1);
        } else {
            s.stuck = s.stuck.saturating_add(1);
        }
        i = j;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{accuracy, safety};
    use crate::record::Record;
    use crate::testkit::{rec, Health, Svc};

    fn labeled(mut r: Record<Svc>, c: Health) -> Record<Svc> {
        r.label = Some(c);
        r
    }

    #[test]
    fn detection_buckets_windows_detected_missed_silent() {
        let ov = Health::Overloaded;
        let records = vec![
            // W1: transition edge (unreachable) then the real peak (overloaded) -> DETECTED
            labeled(rec(1, 100, Health::Unreachable), ov),
            labeled(rec(2, 160, ov), ov),
            rec(3, 220, Health::Ok),
            // W2: dry-run marker, never broke -> SILENT
            labeled(rec(4, 280, Health::Ok), ov),
            rec(5, 340, Health::Ok),
            // W3: wrong class only -> MISSED
            labeled(rec(6, 400, Health::Unreachable), ov),
            rec(7, 460, Health::Ok),
        ];
        let stats: Vec<_> = accuracy(&records).collect();
        let s = stats.iter().find(|s| s.class == ov);
        assert!(matches!(s, Some(st)
            if st.windows == 3 && st.detected == 1 && st.silent == 1 && st.missed == 1 && st.records == 4));
    }

    #[test]
    fn detection_scores_each_class_independently() {
        let records = vec![
            labeled(rec(1, 100, Health::Overloaded), Health::Overloaded),
            rec(2, 160, Health::Ok),
            labeled(rec(3, 220, Health::Unreachable), Health::Unreachable),
            rec(4, 280, Health::Ok),
        ];
        let stats: Vec<_> = accuracy(&records).collect();
        assert_eq!(stats.len(), 2);
        assert!(stats.iter().all(|s| s.windows == 1 && s.detected == 1));
    }

    #[test]
    fn safety_distinguishes_recovered_stuck_and_pending() {
        let ov = Health::Overloaded;
        let records = vec![
            rec(0, 100, Health::Ok),
            labeled(rec(1, 160, Health::Unreachable), ov), // A: transition inside
            labeled(rec(2, 220, ov), ov),                  // A: peak
            rec(3, 280, Health::Ok),                       // A: recovered
            rec(4, 340, Health::Ok),
            labeled(rec(5, 400, ov), ov),     // B
            rec(6, 460, Health::Unreachable), // B: still faulty
            labeled(rec(7, 520, ov), ov),     // C at the tail
        ];
        let s = safety(&records);
        assert_eq!(s.windows, 3);
        assert_eq!(s.recovered, 1);
        assert_eq!(s.stuck, 1);
        assert_eq!(s.pending, 1);
    }
}
