//! The supervision runtime: bind the pure core to concrete ports and drive it in
//! a bounded loop.
//!
//! This is where policy meets the world, but the world is still held at arm's
//! length — telemetry, actuation, notification, and even sleeping are all
//! injected (the principle). Two disciplines from the set of principles live here explicitly:
//! **graceful degradation** (a failed probe becomes an all-`Unknown` observation,
//! never a crash) and **bounded execution** (each cycle does a fixed amount of
//! timeout-guarded work and the loop can be capped).

use sovereign_core::domain::{Dns, NodeId, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
use sovereign_core::error::ActError;
use sovereign_core::failure::classify;
use sovereign_core::fleet::{refine_tunnel, Fleet};
use sovereign_core::fsm::{decide, Action, Config, Reason, State};
use sovereign_core::ports::{Actuator, Notifier, TelemetrySource};
use sovereign_core::record::{Outcome, Record};
use std::time::Duration;

/// Ties the core state machine to its ports. Holds the only mutable state in the
/// system, and holds it explicitly (the principle: no hidden globals).
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
    pub fn new(telemetry: T, actuator: A, notifier: N, cfg: Config) -> Self {
        Self {
            telemetry,
            actuator,
            notifier,
            cfg,
            state: State::Healthy,
            tick: 0,
            fleet: Fleet::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Run exactly one supervision cycle and return a full [`Record`] of it.
    pub fn step(&mut self, now_unix: u64) -> Record {
        let now = Tick(self.tick);
        let (observed, probed) = match self.telemetry.sample() {
            Ok(observation) => (observation, true),
            // Graceful degradation: an unobservable system is treated as all
            // Unknown, which classifies as "no fault" — so a probe outage can
            // never, by itself, trigger a disruptive action.
            Err(_) => (all_unknown(), false),
        };

        // Adaptive perception: feed the observation into the learned models, then
        // let the path's own baseline refine the tunnel signal. A path that is
        // throttled or abnormally slow *for itself* is escalated to Degraded even
        // when its absolute latency is still under the static budget — this is the
        // intelligence the fixed threshold cannot provide.
        let path_latency = match observed.tunnel {
            Tunnel::Healthy { latency } | Tunnel::Degraded { latency } => Some(latency),
            Tunnel::Dead | Tunnel::Unknown => None,
        };
        let verdict = self.fleet.observe_path(path_latency);
        self.fleet.observe_nodes(&observed.nodes);
        let refined = Telemetry {
            tunnel: refine_tunnel(observed.tunnel, verdict),
            ..observed
        };

        // Same pure classification the decision uses — recorded so the trail shows
        // exactly what the machine reasoned about.
        let failure = classify(&refined, self.cfg.degraded_above);
        let transition = decide(self.state, &refined, &self.cfg, now);
        self.state = transition.next;

        let outcome = match self.apply(transition.action) {
            None => Outcome::NotActed,
            Some(Ok(())) => Outcome::Verified,
            Some(Err(_)) => Outcome::Unverified,
        };

        if is_notable(transition.reason) {
            // A failed push is logged by the caller and otherwise ignored: the
            // notifier is a courtesy and must never disturb the core (the principle).
            let _ = self.notifier.notify(transition.reason);
        }

        self.tick = self.tick.saturating_add(1);
        // The record carries the *raw* observation (ground truth) plus the learned
        // verdict, so a future model sees both the signal and the adaptive read.
        Record {
            tick: now,
            wall_time: now_unix,
            probed,
            telemetry: observed,
            path_verdict: verdict,
            failure,
            state: transition.next,
            reason: transition.reason,
            action: transition.action,
            outcome,
        }
    }

    fn apply(&self, action: Action) -> Option<Result<(), ActError>> {
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
    /// read a real clock (the principle). Returns the number of cycles completed.
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
    use sovereign_core::domain::{Dns, LatencyMs, Proc, Telemetry, Tunnel, Wan, MAX_NODES};
    use sovereign_core::error::{ActError, ProbeError};
    use sovereign_core::fsm::Reason;
    use sovereign_core::fsm::{Action, Config, State};
    use sovereign_core::ports::{Actuator, Notifier, TelemetrySource};
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
        fn sample(&self) -> Result<Telemetry, ProbeError> {
            let i = self.idx.get();
            let out = self
                .samples
                .get(i)
                .copied()
                .unwrap_or_else(|| Ok(healthy()));
            self.idx.set(i.saturating_add(1));
            out
        }
    }

    #[derive(Default)]
    struct RecordingActuator {
        restarts: Cell<u32>,
        switches: Cell<u32>,
        bounces: Cell<u32>,
    }
    impl Actuator for RecordingActuator {
        fn switch_node(&self, _avoid: NodeId) -> Result<(), ActError> {
            self.switches.set(self.switches.get().saturating_add(1));
            Ok(())
        }
        fn restart_singbox(&self) -> Result<(), ActError> {
            self.restarts.set(self.restarts.get().saturating_add(1));
            Ok(())
        }
        fn bounce_wan(&self) -> Result<(), ActError> {
            self.bounces.set(self.bounces.get().saturating_add(1));
            Ok(())
        }
        fn failover_backup(&self) -> Result<(), ActError> {
            Ok(())
        }
        fn reboot(&self) -> Result<(), ActError> {
            Ok(())
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
        );

        let r0 = sup.step(0);
        assert_eq!(r0.action, Action::Observe, "never act on first sight");
        let r1 = sup.step(0);
        assert_eq!(r1.action, Action::RestartSingbox);
        assert_eq!(r1.outcome, Outcome::Verified);
        let r2 = sup.step(0);
        assert_eq!(r2.action, Action::Observe, "cooldown holds");
        let r3 = sup.step(0);
        assert_eq!(r3.reason, Reason::Recovered);
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
        );

        let mut switched = false;
        for _ in 0..9 {
            if sup.step(0).action == Action::SwitchNode {
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
        );
        let r = sup.step(0);
        assert!(!r.probed);
        assert_eq!(r.action, Action::Observe);
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
