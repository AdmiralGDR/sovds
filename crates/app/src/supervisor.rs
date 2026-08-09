// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The supervision runtime: bind the pure core to concrete ports and drive it in
//! a bounded loop.
//!
//! This is where policy meets the world, but the world is still held at arm's
//! length — telemetry, actuation, notification, and even sleeping are all
//! injected. Two disciplines from the set of principles live here explicitly:
//! **graceful degradation** (a failed probe becomes an all-`Unknown` observation,
//! never a crash) and **bounded execution** (each cycle does a fixed amount of
//! timeout-guarded work and the loop can be capped).

use sovereign_core::domain::{Aux, Dns, NodeId, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
use sovereign_core::error::ActError;
use sovereign_core::fleet::{Fleet, FleetConfig};
use sovereign_core::fsm::{Action, Config, Reason, State};
use sovereign_core::pipeline::decide_cycle;
use sovereign_core::ports::{Actuation, Actuator, Notifier, TelemetrySource};
use sovereign_core::record::{Outcome, Record, RouterAnnot, Source};
use std::time::Duration;

/// Ties the core state machine to its ports. Holds the only mutable state in the
/// system, and holds it explicitly (no hidden globals).
pub struct Supervisor<T, A, N> {
    telemetry: T,
    actuator: A,
    notifier: N,
    cfg: Config,
    state: State,
    tick: u64,
    /// Adaptive perception: learns each path/node's own baseline online so the
    /// classifier can flag relative degradation, not just absolute thresholds.
    fleet: Fleet,
}

impl<T: TelemetrySource, A: Actuator, N: Notifier> Supervisor<T, A, N> {
    #[must_use]
    pub fn new(
        telemetry: T,
        actuator: A,
        notifier: N,
        cfg: Config,
        fleet_cfg: FleetConfig,
    ) -> Self {
        Self {
            telemetry,
            actuator,
            notifier,
            cfg,
            state: State::Healthy,
            tick: 0,
            fleet: Fleet::new(fleet_cfg),
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Run exactly one supervision cycle and return a full [`Record`] of it.
    pub fn step(&mut self, now_unix: u64) -> Record {
        let now = Tick(self.tick);
        let (observed, injected, probed) = match self.telemetry.sample() {
            Ok(obs) => (obs.telemetry, obs.injected, true),
            // Graceful degradation: an unobservable system is treated as all
            // Unknown, which classifies as "no fault" — so a probe outage can
            // never, by itself, trigger a disruptive action.
            Err(_) => (all_unknown(), None, false),
        };

        // One cycle of pure decision logic (shared with the offline replay tool),
        // advancing the adaptive models and returning the decision.
        let cycle = decide_cycle(self.state, &mut self.fleet, &observed, &self.cfg, now);
        self.state = cycle.next_state;

        let outcome = match self.apply(cycle.action) {
            None => Outcome::NotActed,
            Some(Ok(Actuation::Applied)) => Outcome::Verified,
            // Observe-only: the action was decided but never issued — record it as
            // such, never as a verified actuation the dataset would trust.
            Some(Ok(Actuation::Simulated)) => Outcome::Simulated,
            Some(Err(_)) => Outcome::Unverified,
        };

        if is_notable(cycle.reason) {
            // The notifier is a courtesy and must never disturb the core, but a
            // failure still has to reach someone — silently discarding it left no
            // trace at all that a push was ever attempted.
            if let Err(err) = self.notifier.notify(cycle.reason) {
                eprintln!("sovereign: notification failed ({err:?}); continuing");
            }
        }

        self.tick = self.tick.saturating_add(1);
        // The record carries the *raw* observation (ground truth) plus the learned
        // verdict, so a future model sees both the signal and the adaptive read.
        Record {
            tick: now,
            wall_time: now_unix,
            probed,
            obs: observed,
            class: cycle.failure,
            annot: RouterAnnot {
                verdict: cycle.verdict,
                state: cycle.next_state,
                reason: cycle.reason,
                action: cycle.action,
                outcome,
            },
            source: Source::Live,
            // Ground truth: whether this cycle's fault was one we injected.
            label: injected,
        }
    }

    fn apply(&self, action: Action) -> Option<Result<Actuation, ActError>> {
        match action {
            Action::Observe => None,
            Action::SwitchNode => Some(self.actuator.switch_node(NodeId(0))),
            Action::RestartSingbox => Some(self.actuator.restart_singbox()),
            Action::BounceWan => Some(self.actuator.bounce_wan()),
            Action::FailoverBackup => Some(self.actuator.failover_backup()),
            Action::Reboot => Some(self.actuator.reboot()),
        }
    }

    /// Drive the loop. `budget` caps the cycle count (`None` runs as a service);
    /// `clock`, `sleep`, and `observe` are all injected so tests neither block nor
    /// read a real clock. Returns the number of cycles completed.
    pub fn run<C, S, O>(
        &mut self,
        budget: Option<u64>,
        interval: Duration,
        clock: C,
        sleep: S,
        mut observe: O,
    ) -> u64
    where
        C: Fn() -> u64,
        S: Fn(Duration),
        O: FnMut(&Record),
    {
        let mut completed: u64 = 0;
        loop {
            if let Some(max) = budget {
                if completed >= max {
                    break;
                }
            }
            let report = self.step(clock());
            observe(&report);
            completed = completed.saturating_add(1);
            // Sleep between cycles, but not after the final one.
            if budget != Some(completed) {
                sleep(interval);
            }
        }
        completed
    }
}

/// The observation used when the router cannot be reached: nothing is known, so
/// nothing is a fault.
fn all_unknown() -> Telemetry {
    Telemetry {
        wan: Wan::Unknown,
        singbox: Proc::Unknown,
        tunnel: Tunnel::Unknown,
        dns: Dns::Unknown,
        nodes: [None; MAX_NODES],
        aux: Aux::UNKNOWN,
    }
}

/// Whether a transition is worth pushing to a human. Confirmations, escalations,
/// and recoveries are; routine watching and cooldowns are not (avoid noise).
fn is_notable(reason: Reason) -> bool {
    matches!(
        reason,
        Reason::Confirmed { .. } | Reason::Escalated { .. } | Reason::Recovered
    )
}

#[cfg(test)]
mod tests {
    use super::Supervisor;
    use sovereign_core::domain::NodeId;
    use sovereign_core::domain::{Aux, Dns, LatencyMs, Proc, Telemetry, Tunnel, Wan, MAX_NODES};
    use sovereign_core::error::{ActError, ProbeError};
    use sovereign_core::fleet::FleetConfig;
    use sovereign_core::fsm::Reason;
    use sovereign_core::fsm::{Action, Config, State};
    use sovereign_core::ports::{Actuation, Actuator, Notifier, Observation, TelemetrySource};
    use sovereign_core::record::Outcome;
    use std::cell::Cell;
    use std::time::Duration;

    fn healthy() -> Telemetry {
        Telemetry {
            wan: Wan::Up { uptime_s: 100 },
            singbox: Proc::Running,
            tunnel: Tunnel::Healthy {
                latency: LatencyMs(50),
            },
            dns: Dns::Ok,
            nodes: [None; MAX_NODES],
            aux: Aux::UNKNOWN,
        }
    }

    fn singbox_dead() -> Telemetry {
        let mut t = healthy();
        t.singbox = Proc::Dead;
        t
    }

    fn with_tunnel_latency(ms: u32) -> Telemetry {
        let mut t = healthy();
        t.tunnel = Tunnel::Healthy {
            latency: LatencyMs(ms),
        };
        t
    }

    /// Replays a fixed script of samples; past the end it returns healthy.
    struct ScriptedTelemetry {
        samples: Vec<Result<Telemetry, ProbeError>>,
        idx: Cell<usize>,
    }
    impl TelemetrySource for ScriptedTelemetry {
        fn sample(&self) -> Result<Observation, ProbeError> {
            let i = self.idx.get();
            let out = self
                .samples
                .get(i)
                .copied()
                .unwrap_or_else(|| Ok(healthy()));
            self.idx.set(i.saturating_add(1));
            out.map(Observation::organic)
        }
    }

    #[derive(Default)]
    struct RecordingActuator {
        restarts: Cell<u32>,
        switches: Cell<u32>,
        bounces: Cell<u32>,
    }
    // Represents a real, live actuator: it "applies" each remedy, so the outcome
    // is Verified — the counterpart to NullActuator's Simulated.
    impl Actuator for RecordingActuator {
        fn switch_node(&self, _avoid: NodeId) -> Result<Actuation, ActError> {
            self.switches.set(self.switches.get().saturating_add(1));
            Ok(Actuation::Applied)
        }
        fn restart_singbox(&self) -> Result<Actuation, ActError> {
            self.restarts.set(self.restarts.get().saturating_add(1));
            Ok(Actuation::Applied)
        }
        fn bounce_wan(&self) -> Result<Actuation, ActError> {
            self.bounces.set(self.bounces.get().saturating_add(1));
            Ok(Actuation::Applied)
        }
        fn failover_backup(&self) -> Result<Actuation, ActError> {
            Ok(Actuation::Applied)
        }
        fn reboot(&self) -> Result<Actuation, ActError> {
            Ok(Actuation::Applied)
        }
    }

    #[derive(Default)]
    struct CountingNotifier {
        count: Cell<u32>,
    }
    impl Notifier for CountingNotifier {
        fn notify(&self, _event: Reason) -> Result<(), ActError> {
            self.count.set(self.count.get().saturating_add(1));
            Ok(())
        }
    }

    fn cfg() -> Config {
        Config {
            degraded_above: LatencyMs(300),
            confirm_threshold: 2,
            cooldown_ticks: 2,
        }
    }

    #[test]
    fn confirms_then_acts_then_recovers() {
        let telemetry = ScriptedTelemetry {
            samples: vec![
                Ok(singbox_dead()), // tick0: first suspicion, no action
                Ok(singbox_dead()), // tick1: confirmed -> RestartSingbox, enter cooldown
                Ok(singbox_dead()), // tick2: cooling down, no action
                Ok(healthy()),      // tick3: recovered
            ],
            idx: Cell::new(0),
        };
        let mut sup = Supervisor::new(
            telemetry,
            RecordingActuator::default(),
            CountingNotifier::default(),
            cfg(),
            FleetConfig::legacy(),
        );

        let r0 = sup.step(0);
        assert_eq!(r0.annot.action, Action::Observe, "never act on first sight");
        let r1 = sup.step(0);
        assert_eq!(r1.annot.action, Action::RestartSingbox);
        assert_eq!(r1.annot.outcome, Outcome::Verified);
        let r2 = sup.step(0);
        assert_eq!(r2.annot.action, Action::Observe, "cooldown holds");
        let r3 = sup.step(0);
        assert_eq!(r3.annot.reason, Reason::Recovered);
        assert_eq!(sup.state(), State::Healthy);
    }

    #[test]
    fn learns_a_throttle_below_the_static_threshold() {
        // Every latency here stays far under the 300 ms static budget, so the
        // fixed classifier alone would see nothing. But after the path's baseline
        // settles near 50 ms, a sustained jump to 150 ms is abnormal *for this
        // path*, and the adaptive layer must surface it as a degradation and
        // trigger a node switch.
        let mut samples = vec![Ok(with_tunnel_latency(50)); 5];
        samples.extend(vec![Ok(with_tunnel_latency(150)); 4]);
        let telemetry = ScriptedTelemetry {
            samples,
            idx: Cell::new(0),
        };
        let mut sup = Supervisor::new(
            telemetry,
            RecordingActuator::default(),
            CountingNotifier::default(),
            cfg(),
            FleetConfig::legacy(),
        );

        let mut switched = false;
        for _ in 0..9 {
            if sup.step(0).annot.action == Action::SwitchNode {
                switched = true;
            }
        }
        assert!(
            switched,
            "a relative throttle under the static budget must trigger a switch"
        );
    }

    #[test]
    fn probe_failure_degrades_gracefully() {
        let telemetry = ScriptedTelemetry {
            samples: vec![Err(ProbeError::Unreachable)],
            idx: Cell::new(0),
        };
        let mut sup = Supervisor::new(
            telemetry,
            RecordingActuator::default(),
            CountingNotifier::default(),
            cfg(),
            FleetConfig::legacy(),
        );
        let r = sup.step(0);
        assert!(!r.probed);
        assert_eq!(r.annot.action, Action::Observe);
        assert_eq!(sup.state(), State::Healthy);
    }

    #[test]
    fn run_respects_budget_and_sleeps_between_cycles() {
        let telemetry = ScriptedTelemetry {
            samples: Vec::new(),
            idx: Cell::new(0),
        };
        let mut sup = Supervisor::new(
            telemetry,
            RecordingActuator::default(),
            CountingNotifier::default(),
            cfg(),
            FleetConfig::legacy(),
        );
        let sleeps = Cell::new(0u32);
        let completed = sup.run(
            Some(3),
            Duration::from_millis(0),
            || 0,
            |_| sleeps.set(sleeps.get().saturating_add(1)),
            |_| {},
        );
        assert_eq!(completed, 3);
        assert_eq!(sleeps.get(), 2, "sleep between cycles, not after the last");
    }
}
