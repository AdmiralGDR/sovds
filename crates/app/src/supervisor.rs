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
use sovereign_core::failure::FailureClass;
use sovereign_core::fleet::{Fleet, FleetConfig, NodeVerdict};
use sovereign_core::fsm::{Action, Config, Reason, State};
use sovereign_core::pipeline::{decide_cycle, CycleOutcome};
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
    /// Ticks at which a disruptive action was applied (ring, oldest first).
    recent_actions: [u64; 32],
    recent_len: u8,
    /// One-shot flags so cap / dark notices do not spam.
    cap_notified: bool,
    dark_notified: bool,
    /// Consecutive cycles where the probe failed.
    unobserved_streak: u8,
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
            recent_actions: [0; 32],
            recent_len: 0,
            cap_notified: false,
            dark_notified: false,
            unobserved_streak: 0,
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
            // Unknown for the *record*, but the FSM state is frozen — a probe
            // outage must neither recover nor escalate.
            Err(_) => (all_unknown(), None, false),
        };

        if !probed {
            return self.step_unobserved(now, now_unix, observed);
        }
        self.unobserved_streak = 0;
        self.dark_notified = false;

        // One cycle of pure decision logic (shared with the offline replay tool),
        // advancing the adaptive models and returning the decision.
        let mut cycle = decide_cycle(self.state, &mut self.fleet, &observed, &self.cfg, now);

        // Sliding-window action cap: when hit, hold Observe and notify once.
        if is_disruptive(cycle.action) && self.cap_reached(now.0) {
            cycle.action = Action::Observe;
            cycle.reason = Reason::ActionCapReached;
            // Keep next_state from the decision (cooldown/stabilizing) so the
            // ladder does not climb while we are capped; only suppress actuation.
            if !self.cap_notified {
                self.note(Reason::ActionCapReached);
                self.cap_notified = true;
            }
            self.state = cycle.next_state;
            return self.finish(
                now,
                now_unix,
                true,
                observed,
                injected,
                cycle,
                Outcome::NotActed,
            );
        }
        if is_disruptive(cycle.action) {
            self.record_action(now.0);
            self.cap_notified = false;
        }

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
            self.note(cycle.reason);
        }

        self.finish(now, now_unix, true, observed, injected, cycle, outcome)
    }

    fn step_unobserved(&mut self, now: Tick, now_unix: u64, observed: Telemetry) -> Record {
        self.unobserved_streak = self.unobserved_streak.saturating_add(1);
        let threshold = self.cfg.unobserved_notify_after;
        let reason = if threshold > 0 && self.unobserved_streak == threshold && !self.dark_notified
        {
            self.dark_notified = true;
            let r = Reason::TelemetryDark {
                cycles: self.unobserved_streak,
            };
            self.note(r);
            r
        } else {
            Reason::CoolingDown
        };
        let cycle = CycleOutcome {
            next_state: self.state,
            reason,
            action: Action::Observe,
            verdict: NodeVerdict::Warming,
            failure: FailureClass::None,
        };
        self.finish(
            now,
            now_unix,
            false,
            observed,
            None,
            cycle,
            Outcome::NotActed,
        )
    }

    fn note(&self, reason: Reason) {
        if let Err(err) = self.notifier.notify(reason) {
            eprintln!("sovereign: notification failed ({err:?}); continuing");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        now: Tick,
        now_unix: u64,
        probed: bool,
        observed: Telemetry,
        injected: Option<FailureClass>,
        cycle: CycleOutcome,
        outcome: Outcome,
    ) -> Record {
        self.tick = self.tick.saturating_add(1);
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
            label: injected,
        }
    }

    fn cap_reached(&self, now_tick: u64) -> bool {
        let cap = self.cfg.action_cap;
        let window = self.cfg.action_cap_window;
        if cap == 0 || window == 0 {
            return false;
        }
        let start = now_tick.saturating_sub(window.saturating_sub(1));
        let mut n = 0u8;
        let len = usize::from(self.recent_len).min(self.recent_actions.len());
        for slot in self.recent_actions.iter().take(len) {
            if *slot >= start {
                n = n.saturating_add(1);
            }
        }
        n >= cap
    }

    fn record_action(&mut self, tick: u64) {
        let len = usize::from(self.recent_len);
        if len < self.recent_actions.len() {
            if let Some(slot) = self.recent_actions.get_mut(len) {
                *slot = tick;
                self.recent_len = self.recent_len.saturating_add(1);
            }
            return;
        }
        // Drop the oldest.
        self.recent_actions.copy_within(1.., 0);
        if let Some(last) = self.recent_actions.last_mut() {
            *last = tick;
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
        Reason::Confirmed { .. }
            | Reason::Escalated { .. }
            | Reason::Recovered
            | Reason::LinkDown
            | Reason::ActionCapReached
            | Reason::TelemetryDark { .. }
    )
}

fn is_disruptive(action: Action) -> bool {
    !matches!(action, Action::Observe)
}

#[cfg(test)]
#[path = "supervisor_scenarios.rs"]
mod scenarios;

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
        let mut c = Config::classic(LatencyMs(300), 2, 2);
        // Unit tests below expect one healthy cycle to clear (legacy hysteresis).
        c.recover_stable_ticks = 1;
        c.action_cap = 0;
        c.unobserved_notify_after = 0;
        c
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
