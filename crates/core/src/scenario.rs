// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical fault scenarios — a labelled ground-truth suite for the rules.
//!
//! Passive observation of a healthy network never produces the rare faults
//! (`wan_down`, `singbox_dead`, `dns_fail`): the dataset has zero of them, so we
//! cannot know whether the rules would even diagnose one correctly if it struck.
//! This module closes that blind spot **without touching the live router**: each
//! scenario is a deterministic, hand-built episode (baseline → fault → recovery)
//! that is replayed through the *same* [`decide_cycle`] the live loop uses, and
//! scored against the class it is known to represent.
//!
//! This is a measurement, not a theory: [`evaluate`] reports, per
//! scenario, how many fault-phase cycles the rules classified correctly, when the
//! first remedy fired, and whether any action leaked into a healthy phase. It is
//! also a compile-time guard: the accompanying tests fail the build if
//! the rules stop diagnosing a canonical fault.
//!
//! Everything here is pure, `no_std`, allocation-free, and deterministic —
//! variation across frames is a fixed function of the frame index, never a clock
//! or an RNG, so a scenario replays byte-identically anywhere.

use crate::domain::{
    Aux, Dns, LatencyMs, NodeHealth, NodeId, Proc, Telemetry, Tick, Tunnel, Wan, WanReach,
    MAX_NODES,
};
use crate::failure::FailureClass;
use crate::fleet::{Fleet, FleetConfig};
use crate::fsm::{Action, Config, State};
use crate::pipeline::{decide_cycle, CycleOutcome};

/// Frames of settled-healthy telemetry before the fault, long enough to prime the
/// adaptive baselines (`FleetConfig::tuned().warmup == 8`).
const WARMUP: u32 = 12;
/// Frames the fault is held — long enough to clear hysteresis (`confirm_threshold`
/// defaults to 5) and let a remedy fire.
const FAULT: u32 = 10;
/// Frames of healthy telemetry after the fault clears (proves the rules settle
/// back without acting spuriously).
const RECOVERY: u32 = 6;

/// A settled node's baseline round-trip, in ms — comfortably under the fixed
/// `degraded_above` budget so the *adaptive* layer, not the static threshold, is
/// what must catch a relative slow-down.
const BASE_LAT: u32 = 220;

/// The total frame count of every scenario.
#[must_use]
pub fn length() -> u32 {
    WARMUP.saturating_add(FAULT).saturating_add(RECOVERY)
}

/// Whether frame `i` is inside the fault phase.
fn in_fault(i: u32) -> bool {
    i >= WARMUP && i < WARMUP.saturating_add(FAULT)
}

/// Public view of [`in_fault`] for tooling that renders per-frame phases.
#[must_use]
pub fn in_fault_frame(i: u32) -> bool {
    in_fault(i)
}

/// Offset of frame `i` within the fault phase (0 at fault onset).
fn fault_offset(i: u32) -> u32 {
    i.saturating_sub(WARMUP)
}

/// Deterministic small per-frame variation (0..=15) so replays carry realistic
/// jitter without any clock or RNG.
const fn jitter(i: u32) -> u32 {
    i.wrapping_mul(37) & 15
}

/// One node-1 exit measured at `latency` (or missing when `None`).
fn one_node(latency: Option<LatencyMs>) -> [Option<NodeHealth>; MAX_NODES] {
    let mut nodes = [None; MAX_NODES];
    if let Some(cell) = nodes.get_mut(0) {
        *cell = Some(NodeHealth {
            id: NodeId(1),
            latency,
        });
    }
    nodes
}

/// A fully-healthy observation for frame `i`: WAN up and reachable, tunnel within
/// budget, DNS ok, router unloaded. Every fault template starts from this and
/// perturbs exactly one layer.
fn healthy(i: u32) -> Telemetry {
    let latency = LatencyMs(BASE_LAT.saturating_add(jitter(i)));
    Telemetry {
        wan: Wan::Up {
            uptime_s: 3600u32.saturating_add(i.saturating_mul(60)),
        },
        singbox: Proc::Running,
        tunnel: Tunnel::Healthy { latency },
        dns: Dns::Ok,
        nodes: one_node(Some(latency)),
        aux: Aux {
            wan_reach: WanReach::Ok,
            load_centi: Some(120u32.saturating_add(jitter(i))),
            mem_free_kb: Some(240_000),
            active: Some(1),
        },
    }
}

// --- Fault templates: healthy everywhere except the fault phase, where exactly
// one layer is perturbed to the signature of a single fault class. ---

/// Control: never faults. Proves the rules stay quiet on a healthy stream.
fn baseline(i: u32) -> Telemetry {
    healthy(i)
}

/// WAN link down — the deepest fault; nothing upstream can work.
fn wan_down(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        t.wan = Wan::Down;
        t.tunnel = Tunnel::Dead;
        t.nodes = one_node(None);
    }
    t
}

/// WAN session *up* but blackholed: no external host answers, yet the session
/// status and a stale tunnel probe still read healthy. The `wanok=0` aux signal is
/// the only witness. Included to MEASURE a known gap — the current classifier does
/// not consult `wan_reach`, so it will score 0% here (see the test).
fn wan_blackhole(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        t.aux.wan_reach = WanReach::Blackhole;
    }
    t
}

/// The tunnel process is dead.
fn singbox_dead(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        t.singbox = Proc::Dead;
        t.tunnel = Tunnel::Dead;
        t.nodes = one_node(None);
    }
    t
}

/// Name resolution fails though the tunnel path itself is up.
fn dns_fail(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        t.dns = Dns::Failing;
    }
    t
}

/// The selected node carries no traffic though process and WAN are fine.
fn node_dead(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        t.tunnel = Tunnel::Dead;
        t.nodes = one_node(None);
    }
    t
}

/// A step change: the node stays up but jumps to ~600 ms — still under the fixed
/// 800 ms budget, so only the adaptive baseline can flag it as slow-for-itself.
fn node_degraded(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        let latency = LatencyMs(600u32.saturating_add(jitter(i)));
        t.tunnel = Tunnel::Healthy { latency };
        t.nodes = one_node(Some(latency));
    }
    t
}

/// A sustained upward creep (the throttle signature) — a gentle ramp CUSUM should
/// catch well before any fixed threshold would.
fn throttle(i: u32) -> Telemetry {
    let mut t = healthy(i);
    if in_fault(i) {
        let ramp = fault_offset(i).saturating_mul(60);
        let latency = LatencyMs(BASE_LAT.saturating_add(ramp).saturating_add(jitter(i)));
        t.tunnel = Tunnel::Healthy { latency };
        t.nodes = one_node(Some(latency));
    }
    t
}

/// A named, labelled fault episode.
#[derive(Clone, Copy)]
pub struct Scenario {
    /// Stable identifier for the matrix and the seed records.
    pub name: &'static str,
    /// The class the fault phase is built to represent (`None` for the control).
    pub fault_class: FailureClass,
    /// Frame generator: `index -> telemetry`.
    gen: fn(u32) -> Telemetry,
}

impl Scenario {
    /// The telemetry at frame `i`.
    #[must_use]
    pub fn telemetry(&self, i: u32) -> Telemetry {
        (self.gen)(i)
    }
}

/// The canonical suite — one control plus one episode per fault class, and two
/// adaptive-only slow-downs (a step and a ramp) the fixed threshold cannot see.
pub const CATALOG: [Scenario; 8] = [
    Scenario {
        name: "healthy_baseline",
        fault_class: FailureClass::None,
        gen: baseline,
    },
    Scenario {
        name: "wan_down",
        fault_class: FailureClass::WanDown,
        gen: wan_down,
    },
    Scenario {
        name: "wan_blackhole",
        fault_class: FailureClass::WanDown,
        gen: wan_blackhole,
    },
    Scenario {
        name: "singbox_dead",
        fault_class: FailureClass::SingboxDead,
        gen: singbox_dead,
    },
    Scenario {
        name: "dns_fail",
        fault_class: FailureClass::DnsFail,
        gen: dns_fail,
    },
    Scenario {
        name: "node_dead",
        fault_class: FailureClass::NodeDead,
        gen: node_dead,
    },
    Scenario {
        name: "node_degraded",
        fault_class: FailureClass::NodeDegraded,
        gen: node_degraded,
    },
    Scenario {
        name: "throttle",
        fault_class: FailureClass::NodeDegraded,
        gen: throttle,
    },
];

/// How the rules performed on one scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioReport {
    pub name: &'static str,
    /// The class the fault phase was built to represent.
    pub fault_class: FailureClass,
    /// Fault-phase frame count (the denominator for `detected`).
    pub fault_frames: u32,
    /// Fault-phase cycles whose classification matched `fault_class`.
    pub detected: u32,
    /// Fault-phase offset of the first correct classification, if any.
    pub first_detect: Option<u32>,
    /// The first non-`Observe` action, or `Observe` if none fired.
    pub first_action: Action,
    /// Fault-phase offset at which `first_action` fired, if any.
    pub action_at: Option<u32>,
    /// Non-`Observe` actions during the pre-fault warmup. A true false positive —
    /// acting before any fault exists — and must always be zero.
    pub warmup_actions: u32,
    /// Non-`Observe` actions during the post-fault recovery, once telemetry is
    /// healthy again. Non-zero here is *recovery lag*: lingering adaptive suspicion
    /// (a latched CUSUM alarm over a baseline the fault raised) still driving the
    /// escalation ladder after the path is fine. A discovered finding, not a bug in
    /// this suite — see the tests.
    pub recovery_actions: u32,
}

/// Replay `scenario` through the live decision pipeline, invoking `sink` with every
/// cycle's outcome, and return the scored report.
///
/// `sink` is how a caller persists the frames (e.g. as `src=synth` records) without
/// this pure function knowing anything about I/O. [`evaluate`] is the no-op-sink
/// convenience wrapper. A fresh [`Fleet`] and `Healthy` state make each run
/// self-contained and reproducible.
pub fn run<F>(
    scenario: &Scenario,
    cfg: &Config,
    fleet_cfg: FleetConfig,
    mut sink: F,
) -> ScenarioReport
where
    F: FnMut(u32, &Telemetry, &CycleOutcome),
{
    let mut fleet = Fleet::new(fleet_cfg);
    let mut state = State::Healthy;
    let mut detected = 0u32;
    let mut first_detect: Option<u32> = None;
    let mut first_action = Action::Observe;
    let mut action_at: Option<u32> = None;
    let mut warmup_actions = 0u32;
    let mut recovery_actions = 0u32;

    for i in 0..length() {
        let telemetry = scenario.telemetry(i);
        let cycle = decide_cycle(state, &mut fleet, &telemetry, cfg, Tick(u64::from(i)));
        state = cycle.next_state;
        let acted = !matches!(cycle.action, Action::Observe);

        if in_fault(i) {
            if cycle.failure == scenario.fault_class {
                detected = detected.saturating_add(1);
                if first_detect.is_none() {
                    first_detect = Some(fault_offset(i));
                }
            }
            if acted && action_at.is_none() {
                first_action = cycle.action;
                action_at = Some(fault_offset(i));
            }
        } else if acted {
            // Before the fault vs. after it recovered are very different signals.
            if i < WARMUP {
                warmup_actions = warmup_actions.saturating_add(1);
            } else {
                recovery_actions = recovery_actions.saturating_add(1);
            }
        }

        sink(i, &telemetry, &cycle);
    }

    ScenarioReport {
        name: scenario.name,
        fault_class: scenario.fault_class,
        fault_frames: FAULT,
        detected,
        first_detect,
        first_action,
        action_at,
        warmup_actions,
        recovery_actions,
    }
}

/// Score a scenario without persisting its frames.
#[must_use]
pub fn evaluate(scenario: &Scenario, cfg: &Config, fleet_cfg: FleetConfig) -> ScenarioReport {
    run(scenario, cfg, fleet_cfg, |_, _, _| {})
}

#[cfg(test)]
mod tests {
    use super::{evaluate, in_fault, length, ScenarioReport, CATALOG, FAULT, RECOVERY, WARMUP};
    use crate::domain::LatencyMs;
    use crate::failure::FailureClass;
    use crate::fleet::FleetConfig;
    use crate::fsm::{Action, Config};

    /// The live defaults (mirrors `AppConfig::from_env` without pulling in the app).
    fn cfg() -> Config {
        Config {
            degraded_above: LatencyMs(800),
            confirm_threshold: 5,
            cooldown_ticks: 3,
        }
    }

    /// Evaluate every catalog scenario whose name is in `names`, asserting all of
    /// them were actually present (a typo can't silently skip a check). Panic-free
    /// per the crate lints: no `expect`, no indexing.
    fn for_named<F: FnMut(&ScenarioReport)>(names: &[&str], mut check: F) {
        let mut seen = 0usize;
        for scenario in &CATALOG {
            if !names.contains(&scenario.name) {
                continue;
            }
            check(&evaluate(scenario, &cfg(), FleetConfig::tuned()));
            seen = seen.saturating_add(1);
        }
        assert_eq!(seen, names.len(), "some named scenarios were not found");
    }

    /// The bedrock safety property: NO scenario may act before its fault exists.
    /// A remedy during warmup would mean the supervisor attacks a healthy system —
    /// the one thing an automatic actuator must never do.
    #[test]
    fn no_scenario_acts_before_its_fault() {
        for scenario in &CATALOG {
            let report = evaluate(scenario, &cfg(), FleetConfig::tuned());
            assert_eq!(
                report.warmup_actions, 0,
                "{} acted during the healthy warmup",
                scenario.name
            );
        }
    }

    /// The control stays quiet end to end — no action in any phase.
    #[test]
    fn healthy_baseline_never_acts() {
        for_named(&["healthy_baseline"], |report| {
            assert_eq!(report.action_at, None);
            assert_eq!(report.warmup_actions, 0);
            assert_eq!(report.recovery_actions, 0);
            assert_eq!(
                report.detected, FAULT,
                "healthy stream should read None throughout"
            );
        });
    }

    /// The immediate faults are diagnosed on every fault-phase cycle, a remedy
    /// fires within the phase, and — crucially — the rules settle back cleanly:
    /// the moment telemetry recovers, no further action fires. If any regresses,
    /// the build fails here.
    #[test]
    fn immediate_faults_are_diagnosed_remediated_and_settle_cleanly() {
        for_named(
            &["wan_down", "singbox_dead", "dns_fail", "node_dead"],
            |report| {
                assert_eq!(
                    report.detected, FAULT,
                    "{}: expected every fault cycle classified, got {}/{FAULT}",
                    report.name, report.detected
                );
                assert!(
                    report.action_at.is_some(),
                    "{}: no remedy fired within the fault window",
                    report.name
                );
                assert!(!matches!(report.first_action, Action::Observe));
                assert_eq!(
                    report.recovery_actions, 0,
                    "{}: acted after the fault already cleared",
                    report.name
                );
            },
        );
    }

    /// The adaptive-only slow-downs (step + ramp) are caught by the fleet layer,
    /// below the fixed budget, within a few cycles of hysteresis lag.
    #[test]
    fn adaptive_slowdowns_are_caught_below_the_fixed_budget() {
        for_named(&["node_degraded", "throttle"], |report| {
            assert!(
                report.detected >= FAULT / 2,
                "{}: adaptive detection too weak, {}/{FAULT}",
                report.name,
                report.detected
            );
            assert_eq!(report.fault_class, FailureClass::NodeDegraded);
        });
    }

    /// Recovery lag is now BOUNDED (the `stats::Cusum` bounded-accumulator fix).
    /// Before it, a sustained slow-down wound the CUSUM up without limit and the
    /// alarm latched for 18–31 cycles past recovery, firing remedies on an
    /// already-healthy path (measured on real data: 151 false-degraded verdicts /
    /// 34 disruptive actions over the corpus). After it: at most one drain cycle,
    /// so at most one lingering action even in the worst case of an instant
    /// recovery. The step case (`node_degraded`) settles perfectly clean; the ramp
    /// (`throttle`) keeps the inherent single-frame residual. This pins the fix.
    #[test]
    fn adaptive_recovery_lag_is_bounded() {
        for_named(&["node_degraded", "throttle"], |report| {
            assert!(
                report.recovery_actions <= 1,
                "{}: recovery lag regressed to {} actions (bounded CUSUM should cap \
                 it at 1)",
                report.name,
                report.recovery_actions
            );
        });
        for_named(&["node_degraded"], |report| {
            assert_eq!(
                report.recovery_actions, 0,
                "node_degraded (step recovery) should settle perfectly clean"
            );
        });
    }

    /// KNOWN GAP, asserted so it cannot regress silently: a WAN blackhole (session
    /// up, `wanok=0`) is invisible to the current classifier, which does not read
    /// `wan_reach`. This pins the present behaviour; closing the gap is a separate,
    /// data-driven change to `classify`.
    #[test]
    fn wan_blackhole_is_not_yet_detected() {
        for_named(&["wan_blackhole"], |report| {
            assert_eq!(
                report.detected, 0,
                "blackhole unexpectedly detected — if classify now reads wanok, update this test"
            );
            assert_eq!(report.action_at, None);
        });
    }

    #[test]
    fn phases_partition_the_episode() {
        assert_eq!(
            length(),
            WARMUP.saturating_add(FAULT).saturating_add(RECOVERY)
        );
        assert!(!in_fault(WARMUP.saturating_sub(1)));
        assert!(in_fault(WARMUP));
        assert!(in_fault(WARMUP.saturating_add(FAULT).saturating_sub(1)));
        assert!(!in_fault(WARMUP.saturating_add(FAULT)));
    }
}
