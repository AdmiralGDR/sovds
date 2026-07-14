//! Adaptive per-node perception.
//!
//! Where [`crate::failure::classify`] judges the tunnel against a single static
//! latency budget, the [`Fleet`] here learns **each node's own baseline** online
//! and judges deviations relative to it. That is the intelligence upgrade: a node
//! that has always answered in 50 ms and suddenly sits at 150 ms is flagged as
//! degraded even though 150 ms is still under any sane fixed threshold — and a
//! sustained upward creep (the throttle signature) is caught by CUSUM before a
//! threshold ever would.
//!
//! It holds the only learned state in the system, does so explicitly, and remains
//! pure and deterministic: identical sample streams yield identical verdicts.

use crate::domain::{LatencyMs, NodeHealth, NodeId, Tunnel, MAX_NODES};
use crate::stats::{Cusum, Ewma};

/// Tunable parameters of the adaptive layer. Kept as data, not magic constants,
/// so they can be set from evidence and A/B-compared by replaying real telemetry
/// (the principle: metrics before theories — no blind constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetConfig {
    /// EWMA smoothing denominator for a node's latency baseline (larger = slower).
    pub weight: u64,
    /// CUSUM noise slack, in ms: rises smaller than this are treated as jitter.
    pub slack_ms: u64,
    /// CUSUM alarm threshold, in ms of accumulated drift.
    pub threshold_ms: u64,
    /// Samples required before a node is judged (avoid reacting to a cold model).
    pub warmup: u32,
    /// Consecutive missing measurements that mark a node down.
    pub miss_limit: u32,
    /// "Slow" if a sample exceeds the baseline by more than this many typical
    /// deviations.
    pub slow_factor: u64,
    /// Floor on the typical-deviation estimate, in ms, so a rock-steady node does
    /// not over-react to tiny jitter.
    pub min_spread_ms: u64,
}

impl FleetConfig {
    /// Data-driven defaults, tuned from ~15 h of real telemetry (tunnel latency
    /// p50 ≈ 439 ms, p95 ≈ 461, p99 ≈ 506, with a rare thin tail). The "normal"
    /// band is narrow but noisy, so the original constants over-fired on ordinary
    /// jitter. These widen the tolerance — a wider baseline, CUSUM slack above the
    /// p99–p50 gap (~67 ms), and a higher spread floor — so only a genuine,
    /// sustained departure from a node's own baseline is flagged.
    #[must_use]
    pub const fn tuned() -> Self {
        Self {
            weight: 16,
            slack_ms: 100,
            threshold_ms: 400,
            warmup: 8,
            miss_limit: 4,
            slow_factor: 3,
            min_spread_ms: 50,
        }
    }

    /// The original, pre-tuning constants — kept for A/B replay comparison.
    #[must_use]
    pub const fn legacy() -> Self {
        Self {
            weight: 8,
            slack_ms: 20,
            threshold_ms: 100,
            warmup: 4,
            miss_limit: 3,
            slow_factor: 3,
            min_spread_ms: 10,
        }
    }
}

/// The adaptive verdict for a node or the active path, relative to its own norm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeVerdict {
    /// Too few samples to judge yet.
    Warming,
    /// Behaving within its learned baseline.
    Healthy,
    /// Abnormally slow for itself (a wide, one-off-shaped deviation).
    Slow,
    /// A sustained upward latency shift — throttling suspected.
    Throttled,
    /// No measurement for too long.
    Down,
}

/// An online model of one latency stream (a node, or the active tunnel path).
/// Carries its own [`FleetConfig`] so a model always judges by the parameters it
/// was built with.
#[derive(Debug, Clone, Copy)]
pub struct NodeModel {
    mean: Ewma,
    spread: Ewma,
    cusum: Cusum,
    count: u32,
    misses: u32,
    verdict: NodeVerdict,
    cfg: FleetConfig,
}

impl NodeModel {
    /// A fresh, un-primed model parameterised by `cfg`.
    #[must_use]
    pub const fn new(cfg: FleetConfig) -> Self {
        Self {
            mean: Ewma::new(cfg.weight),
            spread: Ewma::new(cfg.weight),
            cusum: Cusum::new(cfg.slack_ms, cfg.threshold_ms),
            count: 0,
            misses: 0,
            verdict: NodeVerdict::Warming,
            cfg,
        }
    }

    /// Fold in this cycle's measurement (`None` == no successful probe) and return
    /// the updated verdict.
    pub fn observe(&mut self, latency: Option<LatencyMs>) -> NodeVerdict {
        match latency {
            None => {
                self.misses = self.misses.saturating_add(1);
                if self.misses >= self.cfg.miss_limit {
                    self.verdict = NodeVerdict::Down;
                }
                self.verdict
            }
            Some(LatencyMs(raw)) => {
                self.misses = 0;
                let sample = u64::from(raw);
                self.verdict = match self.mean.get() {
                    // First real sample: seed the baseline, stay warming.
                    None => {
                        self.mean.update(sample);
                        self.count = self.count.saturating_add(1);
                        NodeVerdict::Warming
                    }
                    Some(baseline) => {
                        let deviation = sample.abs_diff(baseline);
                        self.spread.update(deviation);
                        let throttling = self.cusum.update(sample, baseline);
                        self.count = self.count.saturating_add(1);
                        self.mean.update(sample);
                        self.judge(sample, baseline, throttling)
                    }
                };
                self.verdict
            }
        }
    }

    fn judge(&self, sample: u64, baseline: u64, throttling: bool) -> NodeVerdict {
        if self.count < self.cfg.warmup {
            return NodeVerdict::Warming;
        }
        if throttling {
            return NodeVerdict::Throttled;
        }
        let typical = self.spread.get().unwrap_or(0).max(self.cfg.min_spread_ms);
        // Only an *upward* excursion counts as slow; faster-than-usual is fine.
        let excess = sample.saturating_sub(baseline);
        if excess > typical.saturating_mul(self.cfg.slow_factor) {
            NodeVerdict::Slow
        } else {
            NodeVerdict::Healthy
        }
    }

    /// The learned baseline latency, if primed.
    #[must_use]
    pub fn baseline(&self) -> Option<u64> {
        self.mean.get()
    }

    /// The most recent verdict.
    #[must_use]
    pub fn verdict(&self) -> NodeVerdict {
        self.verdict
    }
}

/// The learned state of the whole tunnel: one model per exit node plus a model of
/// the currently selected path. Fixed capacity keeps it allocation-free and every
/// scan bounded (the principle & IV).
#[derive(Debug, Clone, Copy)]
pub struct Fleet {
    nodes: [NodeModel; MAX_NODES],
    seen: [bool; MAX_NODES],
    path: NodeModel,
}

impl Default for Fleet {
    fn default() -> Self {
        Self::new(FleetConfig::tuned())
    }
}

impl Fleet {
    /// An empty fleet with no models primed, parameterised by `cfg`.
    #[must_use]
    pub const fn new(cfg: FleetConfig) -> Self {
        Self {
            nodes: [NodeModel::new(cfg); MAX_NODES],
            seen: [false; MAX_NODES],
            path: NodeModel::new(cfg),
        }
    }

    /// Fold this cycle's active-path latency into the path model and return its
    /// verdict. `None` means the path carried no measurement this cycle.
    pub fn observe_path(&mut self, latency: Option<LatencyMs>) -> NodeVerdict {
        self.path.observe(latency)
    }

    /// Fold this cycle's per-node measurements into their models. Node ids at or
    /// beyond [`MAX_NODES`] are dropped (bounded set), never a panic.
    pub fn observe_nodes(&mut self, nodes: &[Option<NodeHealth>; MAX_NODES]) {
        for health in nodes.iter().flatten() {
            let index = usize::from(health.id.0);
            if let Some(model) = self.nodes.get_mut(index) {
                model.observe(health.latency);
                if let Some(flag) = self.seen.get_mut(index) {
                    *flag = true;
                }
            }
        }
    }

    /// The healthy node with the lowest learned baseline — the best target for a
    /// deliberate switch. `None` if no healthy node has been seen.
    #[must_use]
    pub fn best_node(&self) -> Option<NodeId> {
        let mut best: Option<(usize, u64)> = None;
        for (index, model) in self.nodes.iter().enumerate() {
            let observed = self.seen.get(index).copied().unwrap_or(false);
            if !observed || !matches!(model.verdict(), NodeVerdict::Healthy) {
                continue;
            }
            let baseline = model.baseline().unwrap_or(u64::MAX);
            let better = match best {
                Some((_, incumbent)) => baseline < incumbent,
                None => true,
            };
            if better {
                best = Some((index, baseline));
            }
        }
        best.and_then(|(index, _)| u8::try_from(index).ok().map(NodeId))
    }
}

/// Upgrade the observed tunnel signal using the learned path verdict.
///
/// This is the seam where adaptive perception overrides the static threshold: a
/// path that is throttled or abnormally slow *for itself* is reported as
/// `Degraded` even when its absolute latency is still under the fixed budget.
/// Everything else passes through unchanged, so the pure classifier stays the
/// single source of truth for what a signal *means*.
#[must_use]
pub fn refine_tunnel(observed: Tunnel, verdict: NodeVerdict) -> Tunnel {
    match (observed, verdict) {
        (Tunnel::Healthy { latency }, NodeVerdict::Throttled | NodeVerdict::Slow) => {
            Tunnel::Degraded { latency }
        }
        (other, _) => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{refine_tunnel, Fleet, FleetConfig, NodeModel, NodeVerdict};
    use crate::domain::{LatencyMs, NodeHealth, NodeId, Tunnel, MAX_NODES};

    // Mechanism tests use the sensitive `legacy` config so a clear relative jump
    // is expected to flag; production uses the tuned, less twitchy config.
    fn model() -> NodeModel {
        NodeModel::new(FleetConfig::legacy())
    }

    fn feed(model: &mut NodeModel, samples: &[u32]) -> NodeVerdict {
        let mut last = NodeVerdict::Warming;
        for &s in samples {
            last = model.observe(Some(LatencyMs(s)));
        }
        last
    }

    #[test]
    fn a_steady_node_reads_healthy_after_warmup() {
        let mut m = model();
        let v = feed(&mut m, &[50, 51, 49, 50, 50, 51]);
        assert_eq!(v, NodeVerdict::Healthy);
    }

    #[test]
    fn a_relative_jump_below_any_fixed_threshold_is_flagged() {
        let mut m = model();
        // Baseline ~50 ms, then a jump to 150 ms — far under a 300 ms budget, yet
        // clearly abnormal for THIS node.
        feed(&mut m, &[50, 50, 50, 50, 50]);
        let v = feed(&mut m, &[150, 150]);
        assert!(matches!(v, NodeVerdict::Slow | NodeVerdict::Throttled));
    }

    #[test]
    fn missing_measurements_mark_a_node_down() {
        let mut m = model();
        feed(&mut m, &[50, 50, 50, 50]);
        m.observe(None);
        m.observe(None);
        let v = m.observe(None);
        assert_eq!(v, NodeVerdict::Down);
    }

    #[test]
    fn best_node_picks_the_lowest_healthy_baseline() {
        let mut fleet = Fleet::new(FleetConfig::legacy());
        // Prime nodes 1 (fast) and 2 (slow) over several cycles.
        for _ in 0..6 {
            let mut nodes: [Option<NodeHealth>; MAX_NODES] = [None; MAX_NODES];
            if let Some(cell) = nodes.get_mut(1) {
                *cell = Some(NodeHealth {
                    id: NodeId(1),
                    latency: Some(LatencyMs(40)),
                });
            }
            if let Some(cell) = nodes.get_mut(2) {
                *cell = Some(NodeHealth {
                    id: NodeId(2),
                    latency: Some(LatencyMs(120)),
                });
            }
            fleet.observe_nodes(&nodes);
        }
        assert_eq!(fleet.best_node(), Some(NodeId(1)));
    }

    #[test]
    fn refine_upgrades_only_a_healthy_signal_under_a_bad_verdict() {
        let healthy = Tunnel::Healthy {
            latency: LatencyMs(120),
        };
        assert_eq!(
            refine_tunnel(healthy, NodeVerdict::Throttled),
            Tunnel::Degraded {
                latency: LatencyMs(120)
            }
        );
        // A Healthy verdict leaves it alone.
        assert_eq!(refine_tunnel(healthy, NodeVerdict::Healthy), healthy);
        // A dead path is not "upgraded" to merely degraded.
        assert_eq!(
            refine_tunnel(Tunnel::Dead, NodeVerdict::Throttled),
            Tunnel::Dead
        );
    }
}
