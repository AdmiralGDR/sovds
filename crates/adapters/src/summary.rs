//! Dataset summary: turn a corpus of records into the metrics that decide what to
//! do next (the principle: metrics before theories).
//!
//! It answers the questions that matter before any model or threshold tuning:
//! how much data, how much of it is real (probed), how imbalanced are the fault
//! classes, what does the tunnel latency distribution actually look like, and how
//! available is each exit node. All arithmetic is integer and saturating; this is
//! an offline laptop tool, so `Vec`/sort are used freely.

use core::fmt;
use sovereign_core::domain::MAX_NODES;
use sovereign_core::failure::FailureClass;
use sovereign_core::fsm::Action;
use sovereign_core::record::Record;

/// Latency distribution over a set of samples, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencyStats {
    pub count: usize,
    pub min: u32,
    pub max: u32,
    pub mean: u32,
    pub p50: u32,
    pub p95: u32,
    pub p99: u32,
}

/// Availability and latency of one exit node across the corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    pub id: u8,
    /// Cycles in which the node reported a measurement.
    pub seen: usize,
    pub latency: LatencyStats,
}

/// The whole-corpus summary.
#[derive(Debug, Clone)]
pub struct Summary {
    pub total: usize,
    pub probed: usize,
    /// Number of collector runs in the corpus (each run restarts `tick` at 0), so
    /// the per-run logical clock is never misread as one global timeline.
    pub runs: usize,
    /// Earliest / latest wall-clock stamp (Unix seconds) among timestamped
    /// records; `0` when none carry a timestamp. Gives the real span the data
    /// covers, across restarts.
    pub first_ts: u64,
    pub last_ts: u64,
    /// Fault-class counts, in a fixed order (the imbalance picture).
    pub failures: [(FailureClass, usize); 6],
    /// Action counts, in a fixed order.
    pub actions: [(Action, usize); 6],
    /// Latency of the selected path (`tun=ok:N`) across probed cycles.
    pub tunnel: LatencyStats,
    /// Per-node availability + latency (only nodes ever seen).
    pub nodes: Vec<NodeStat>,
}

/// Compute the summary. Pure over the slice.
#[must_use]
pub fn summarize(records: &[Record]) -> Summary {
    let total = records.len();
    let probed = records.iter().filter(|r| r.probed).count();
    // Each run starts at tick 0; counting those boundaries is meaningful across a
    // log that spans many restarts, unlike a raw min..max of the per-run clock.
    let runs = records.iter().filter(|r| r.tick.0 == 0).count();
    let first_ts = records
        .iter()
        .map(|r| r.wall_time)
        .filter(|&t| t > 0)
        .min()
        .unwrap_or(0);
    let last_ts = records
        .iter()
        .map(|r| r.wall_time)
        .filter(|&t| t > 0)
        .max()
        .unwrap_or(0);

    let failures = [
        FailureClass::None,
        FailureClass::WanDown,
        FailureClass::SingboxDead,
        FailureClass::NodeDead,
        FailureClass::NodeDegraded,
        FailureClass::DnsFail,
    ]
    .map(|kind| (kind, records.iter().filter(|r| r.failure == kind).count()));

    let actions = [
        Action::Observe,
        Action::SwitchNode,
        Action::RestartSingbox,
        Action::BounceWan,
        Action::FailoverBackup,
        Action::Reboot,
    ]
    .map(|kind| (kind, records.iter().filter(|r| r.action == kind).count()));

    let mut path_latencies: Vec<u32> = Vec::new();
    for record in records {
        if let sovereign_core::domain::Tunnel::Healthy { latency } = record.telemetry.tunnel {
            path_latencies.push(latency.0);
        }
    }
    let tunnel = latency_stats(path_latencies);

    let nodes = per_node_stats(records);

    Summary {
        total,
        probed,
        runs,
        first_ts,
        last_ts,
        failures,
        actions,
        tunnel,
        nodes,
    }
}

fn per_node_stats(records: &[Record]) -> Vec<NodeStat> {
    let mut seen = [0usize; MAX_NODES];
    let mut latencies: [Vec<u32>; MAX_NODES] = core::array::from_fn(|_| Vec::new());

    for record in records {
        for node in record.telemetry.nodes.iter().flatten() {
            let idx = usize::from(node.id.0);
            if let Some(count) = seen.get_mut(idx) {
                *count = count.saturating_add(1);
            }
            if let (Some(bucket), Some(latency)) = (latencies.get_mut(idx), node.latency) {
                bucket.push(latency.0);
            }
        }
    }

    let mut out = Vec::new();
    for (idx, count) in seen.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let samples = latencies.get(idx).cloned().unwrap_or_default();
        let id = u8::try_from(idx).unwrap_or(u8::MAX);
        out.push(NodeStat {
            id,
            seen: *count,
            latency: latency_stats(samples),
        });
    }
    out
}

fn latency_stats(mut values: Vec<u32>) -> LatencyStats {
    values.sort_unstable();
    let count = values.len();
    if count == 0 {
        return LatencyStats::default();
    }
    let min = values.first().copied().unwrap_or(0);
    let max = values.last().copied().unwrap_or(0);
    let sum = values
        .iter()
        .fold(0u64, |acc, &v| acc.saturating_add(u64::from(v)));
    let mean = u64_to_u32(sum.checked_div(count as u64).unwrap_or(0));
    LatencyStats {
        count,
        min,
        max,
        mean,
        p50: percentile(&values, 50),
        p95: percentile(&values, 95),
        p99: percentile(&values, 99),
    }
}

/// Nearest-rank percentile of a sorted slice: the value at index
/// `ceil(p*n/100) - 1`, clamped. (For small `n`, high percentiles collapse to the
/// max — expected, and flagged by the reported sample count.)
fn percentile(sorted: &[u32], p: usize) -> u32 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    let rank = p
        .saturating_mul(n)
        .saturating_add(99)
        .checked_div(100)
        .unwrap_or(0);
    let idx = rank.saturating_sub(1).min(n.saturating_sub(1));
    sorted.get(idx).copied().unwrap_or(0)
}

fn u64_to_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn failure_label(f: FailureClass) -> &'static str {
    match f {
        FailureClass::None => "none",
        FailureClass::WanDown => "wan_down",
        FailureClass::SingboxDead => "singbox_dead",
        FailureClass::NodeDead => "node_dead",
        FailureClass::NodeDegraded => "node_degraded",
        FailureClass::DnsFail => "dns_fail",
    }
}

fn action_label(a: Action) -> &'static str {
    match a {
        Action::Observe => "observe",
        Action::SwitchNode => "switch_node",
        Action::RestartSingbox => "restart_singbox",
        Action::BounceWan => "bounce_wan",
        Action::FailoverBackup => "failover_backup",
        Action::Reboot => "reboot",
    }
}

/// Human-readable duration from a span in seconds (e.g. `2d 3h 4m`).
fn fmt_duration(seconds: u64) -> String {
    let days = seconds.checked_div(86_400).unwrap_or(0);
    let hours = seconds
        .checked_div(3_600)
        .unwrap_or(0)
        .checked_rem(24)
        .unwrap_or(0);
    let mins = seconds
        .checked_div(60)
        .unwrap_or(0)
        .checked_rem(60)
        .unwrap_or(0);
    format!("{days}d {hours}h {mins}m")
}

/// Percent of `part` in `whole`, rounded, as an integer 0..100.
fn pct(part: usize, whole: usize) -> usize {
    if whole == 0 {
        return 0;
    }
    part.saturating_mul(100).checked_div(whole).unwrap_or(0)
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "== sovereign dataset summary ==")?;
        writeln!(
            f,
            "records: {}   probed: {} ({}%)   runs: {}",
            self.total,
            self.probed,
            pct(self.probed, self.total),
            self.runs,
        )?;
        if self.last_ts > 0 {
            writeln!(
                f,
                "collected: unix {}..{}  ({})",
                self.first_ts,
                self.last_ts,
                fmt_duration(self.last_ts.saturating_sub(self.first_ts)),
            )?;
        }

        writeln!(f, "\nfault classes (imbalance):")?;
        for (kind, n) in self.failures {
            writeln!(
                f,
                "  {:<14} {:>8}  {:>3}%",
                failure_label(kind),
                n,
                pct(n, self.total)
            )?;
        }

        writeln!(f, "\nactions decided:")?;
        for (kind, n) in self.actions {
            writeln!(
                f,
                "  {:<16} {:>8}  {:>3}%",
                action_label(kind),
                n,
                pct(n, self.total)
            )?;
        }

        writeln!(
            f,
            "\ntunnel latency ms (selected path, n={}):",
            self.tunnel.count
        )?;
        write_latency(f, self.tunnel)?;

        writeln!(f, "\nexit nodes:")?;
        if self.nodes.is_empty() {
            writeln!(f, "  (none observed yet)")?;
        }
        for node in &self.nodes {
            writeln!(
                f,
                "  node {}: seen {} time(s), latency ms n={}",
                node.id, node.seen, node.latency.count
            )?;
            write_latency(f, node.latency)?;
        }
        Ok(())
    }
}

fn write_latency(f: &mut fmt::Formatter<'_>, s: LatencyStats) -> fmt::Result {
    if s.count == 0 {
        return writeln!(f, "    (no samples)");
    }
    writeln!(
        f,
        "    min {}  mean {}  p50 {}  p95 {}  p99 {}  max {}",
        s.min, s.mean, s.p50, s.p95, s.p99, s.max
    )
}

#[cfg(test)]
mod tests {
    use super::{percentile, summarize};
    use sovereign_core::domain::{
        Aux, Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES,
    };
    use sovereign_core::failure::FailureClass;
    use sovereign_core::fleet::NodeVerdict;
    use sovereign_core::fsm::{Action, Reason, State};
    use sovereign_core::record::{Outcome, Record, Source};

    fn rec(tick: u64, tun_ms: u32, failure: FailureClass) -> Record {
        Record {
            tick: Tick(tick),
            wall_time: 1_700_000_000_u64.saturating_add(tick),
            probed: true,
            telemetry: Telemetry {
                wan: Wan::Up { uptime_s: 1 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(tun_ms),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
                aux: Aux::UNKNOWN,
            },
            path_verdict: NodeVerdict::Healthy,
            failure,
            state: State::Healthy,
            reason: Reason::Nominal,
            action: Action::Observe,
            outcome: Outcome::NotActed,
            source: Source::Live,
        }
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let sorted: Vec<u32> = (1..=10).collect(); // 1..10
        assert_eq!(percentile(&sorted, 50), 5); // ceil(5)-1 = idx 4 -> value 5
        assert_eq!(percentile(&sorted, 100), 10);
        assert_eq!(percentile(&sorted, 95), 10); // small-n collapses to max
    }

    #[test]
    fn summary_counts_classes_and_latency() {
        let records = vec![
            rec(0, 400, FailureClass::None),
            rec(1, 500, FailureClass::None),
            rec(2, 600, FailureClass::NodeDegraded),
        ];
        let s = summarize(&records);
        assert_eq!(s.total, 3);
        assert_eq!(s.probed, 3);
        // None appears twice, NodeDegraded once.
        let none = s
            .failures
            .iter()
            .find(|(k, _)| *k == FailureClass::None)
            .map(|(_, n)| *n);
        assert_eq!(none, Some(2));
        assert_eq!(s.tunnel.count, 3);
        assert_eq!(s.tunnel.min, 400);
        assert_eq!(s.tunnel.max, 600);
        assert_eq!(s.tunnel.mean, 500);
    }
}
