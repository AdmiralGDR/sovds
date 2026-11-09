// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sequential failure and recovery scenarios driven through the real
//! [`Supervisor`]: classify, adaptive fleet, state machine, actuation and
//! notification, one cycle at a time. Telemetry is synthetic, the actuator and
//! notifier only record, and ticks stand in for the 30 s cycle, so an hour of
//! supervision runs in microseconds with no clock, network or router involved.
//!
//! The assertions are the bounds the current code promises: no action before
//! `confirm_threshold`, least-disruptive remedy first, cooldown and backoff
//! between remedies, cable-down stops at failover (no reboot), flapping is
//! capped, escalation survives a single good cycle, and an unobservable
//! router freezes state. The journal
//! (`cargo test -p sovereign-app scenario_journal -- --nocapture`) prints
//! the before/after counts for an hour of each sequence.

use super::Supervisor;
use sovereign_core::domain::{
    Aux, Dns, LatencyMs, Link, NodeId, PppPhase, Proc, Telemetry, Tunnel, Wan, WanReach, MAX_NODES,
};
use sovereign_core::error::{ActError, ProbeError};
use sovereign_core::fleet::FleetConfig;
use sovereign_core::fsm::{Action, Config, Reason};
use sovereign_core::ports::{Actuation, Actuator, Notifier, Observation, TelemetrySource};
use std::cell::{Cell, RefCell};

/// The shipped defaults (`SOVEREIGN_DEGRADED_MS`, `SOVEREIGN_CONFIRM`,
/// `SOVEREIGN_COOLDOWN` unset).
const CFG: Config = Config {
    degraded_above: LatencyMs(1500),
    confirm_threshold: 5,
    cooldown_ticks: 3,
    recover_stable_ticks: 3,
    action_cap: 6,
    action_cap_window: 120,
    unobserved_notify_after: 20,
};
/// One hour at the default 30 s interval.
const HOUR: u64 = 120;

type Sample = Result<Telemetry, ProbeError>;

/// The earliest tick a fault first seen at `start` can be acted on.
fn confirmed_at(start: u64) -> u64 {
    start.saturating_add(u64::from(CFG.confirm_threshold).saturating_sub(1))
}

struct Script<F: Fn(u64) -> Sample> {
    at: F,
    tick: Cell<u64>,
}
impl<F: Fn(u64) -> Sample> TelemetrySource for Script<F> {
    fn sample(&self) -> Result<Observation, ProbeError> {
        let t = self.tick.get();
        self.tick.set(t.saturating_add(1));
        (self.at)(t).map(Observation::organic)
    }
}

#[derive(Default)]
struct Log {
    tick: Cell<u64>,
    actions: RefCell<Vec<(u64, Action)>>,
    notes: RefCell<Vec<(u64, Reason)>>,
}
struct Recorder<'a>(&'a Log);
impl Recorder<'_> {
    fn push(&self, a: Action) -> Result<Actuation, ActError> {
        self.0.actions.borrow_mut().push((self.0.tick.get(), a));
        Ok(Actuation::Applied)
    }
}
impl Actuator for Recorder<'_> {
    fn switch_node(&self, _avoid: NodeId) -> Result<Actuation, ActError> {
        self.push(Action::SwitchNode)
    }
    fn restart_singbox(&self) -> Result<Actuation, ActError> {
        self.push(Action::RestartSingbox)
    }
    fn bounce_wan(&self) -> Result<Actuation, ActError> {
        self.push(Action::BounceWan)
    }
    fn failover_backup(&self) -> Result<Actuation, ActError> {
        self.push(Action::FailoverBackup)
    }
    fn reboot(&self) -> Result<Actuation, ActError> {
        self.push(Action::Reboot)
    }
}
impl Notifier for Recorder<'_> {
    fn notify(&self, event: Reason) -> Result<(), ActError> {
        self.0.notes.borrow_mut().push((self.0.tick.get(), event));
        Ok(())
    }
}

/// Run `ticks` cycles of `at` through a fresh supervisor, starting the fake
/// clock at `start`.
fn drive<F: Fn(u64) -> Sample>(log: &Log, start: u64, ticks: u64, at: F) {
    drive_with(log, start, ticks, CFG, at);
}

fn drive_with<F: Fn(u64) -> Sample>(log: &Log, start: u64, ticks: u64, cfg: Config, at: F) {
    let script = Script {
        at,
        tick: Cell::new(start),
    };
    let mut sup = Supervisor::new(
        script,
        Recorder(log),
        Recorder(log),
        cfg,
        FleetConfig::tuned(),
    );
    for t in start..start.saturating_add(ticks) {
        log.tick.set(t);
        let _ = sup.step(t.saturating_mul(30));
    }
}

fn uncapped() -> Config {
    let mut c = CFG;
    c.action_cap = 0;
    c
}

fn healthy() -> Telemetry {
    Telemetry {
        wan: Wan::Up { uptime_s: 3600 },
        singbox: Proc::Running,
        tunnel: Tunnel::Healthy {
            latency: LatencyMs(120),
        },
        dns: Dns::Ok,
        nodes: [None; MAX_NODES],
        aux: Aux {
            wan_reach: WanReach::Ok,
            link: Link::Up,
            ppp: PppPhase::Session,
            ..Aux::UNKNOWN
        },
    }
}
fn singbox_dead() -> Telemetry {
    Telemetry {
        singbox: Proc::Dead,
        tunnel: Tunnel::Dead,
        ..healthy()
    }
}
fn blackhole() -> Telemetry {
    let h = healthy();
    Telemetry {
        aux: Aux {
            wan_reach: WanReach::Blackhole,
            ..h.aux
        },
        ..h
    }
}
fn pppoe_discovery() -> Telemetry {
    let h = healthy();
    Telemetry {
        wan: Wan::Down,
        tunnel: Tunnel::Dead,
        aux: Aux {
            ppp: PppPhase::Discovery,
            wan_reach: WanReach::Blackhole,
            ..h.aux
        },
        ..h
    }
}
fn link_down() -> Telemetry {
    let p = pppoe_discovery();
    Telemetry {
        aux: Aux {
            link: Link::Down,
            ppp: PppPhase::Down,
            ..p.aux
        },
        ..p
    }
}

fn actions(log: &Log) -> Vec<(u64, Action)> {
    log.actions.borrow().clone()
}
fn count(log: &Log, a: Action) -> usize {
    log.actions.borrow().iter().filter(|(_, x)| *x == a).count()
}
fn recovered_notes(log: &Log) -> usize {
    log.notes
        .borrow()
        .iter()
        .filter(|(_, r)| *r == Reason::Recovered)
        .count()
}

/// Bounds every remedy sequence must satisfy under the current state machine.
fn assert_bounded(log: &Log, first_fault: u64, ladder_head: Action) {
    let acts = actions(log);
    let mut it = acts.iter();
    if let Some((t0, a0)) = it.next() {
        let earliest = confirmed_at(first_fault);
        assert!(
            *t0 >= earliest,
            "acted at {t0}, before confirmation at {earliest}"
        );
        assert_eq!(*a0, ladder_head, "first remedy must be the ladder head");
    }
    // Consecutive remedies are at least one cooldown apart, and a gap only
    // shrinks back to the base cooldown after a recovery reset the level.
    for w in acts.windows(2) {
        if let [(a, _), (b, _)] = w {
            assert!(
                b.saturating_sub(*a) >= CFG.cooldown_ticks,
                "remedies at {a} and {b} closer than the cooldown"
            );
        }
    }
}

#[test]
fn transient_faults_below_confirmation_never_act() {
    let log = Log::default();
    // Four failing cycles out of every five, for an hour: never five in a row.
    drive(&log, 0, HOUR, |t| {
        Ok(if t % 5 == 4 {
            healthy()
        } else {
            singbox_dead()
        })
    });
    assert!(actions(&log).is_empty(), "{:?}", actions(&log));
}

#[test]
fn persistent_singbox_death_climbs_once_then_backs_off() {
    let log = Log::default();
    // Uncapped: this test locks the backoff shape, not the hourly ceiling.
    drive_with(&log, 0, HOUR, uncapped(), |_| Ok(singbox_dead()));
    assert_bounded(&log, 0, Action::RestartSingbox);
    assert_eq!(count(&log, Action::RestartSingbox), 1);
    // Gaps between reboots widen with the level: strictly increasing.
    let reboots: Vec<u64> = actions(&log)
        .iter()
        .filter(|(_, a)| *a == Action::Reboot)
        .map(|(t, _)| *t)
        .collect();
    let gaps: Vec<u64> = reboots
        .windows(2)
        .map(|w| match w {
            [a, b] => b.saturating_sub(*a),
            _ => 0,
        })
        .collect();
    assert!(
        gaps.windows(2).all(|g| matches!(g, [x, y] if y > x)),
        "{gaps:?}"
    );
}

#[test]
fn measured_blackhole_walks_the_wan_ladder_in_order() {
    let log = Log::default();
    drive_with(&log, 0, 30, uncapped(), |t| {
        Ok(if t < 2 { healthy() } else { blackhole() })
    });
    assert_bounded(&log, 2, Action::BounceWan);
    let order: Vec<Action> = actions(&log).iter().map(|(_, a)| *a).take(3).collect();
    assert_eq!(
        order,
        [Action::BounceWan, Action::FailoverBackup, Action::Reboot]
    );
}

#[test]
fn unknown_wan_reach_is_not_a_blackhole() {
    let log = Log::default();
    drive(&log, 0, HOUR, |_| {
        let h = healthy();
        Ok(Telemetry {
            aux: Aux::UNKNOWN,
            ..h
        })
    });
    assert!(actions(&log).is_empty());
}

#[test]
fn probe_outage_alone_never_acts_or_notifies() {
    let log = Log::default();
    drive(&log, 0, HOUR, |_| Err(ProbeError::Timeout));
    assert!(actions(&log).is_empty());
    // One TelemetryDark notice at the configured streak; nothing else.
    let notes = log.notes.borrow().clone();
    assert_eq!(notes.len(), 1);
    let only = notes.first().map(|(_, r)| *r);
    assert!(matches!(
        only,
        Some(Reason::TelemetryDark {
            cycles
        }) if cycles == CFG.unobserved_notify_after
    ));
}

#[test]
fn recovery_then_relapse_starts_over_with_hysteresis() {
    let log = Log::default();
    // Fault long enough to escalate once, stable for 20 cycles, then relapse.
    drive(&log, 0, 60, |t| {
        Ok(match t {
            0..=11 => singbox_dead(),
            12..=31 => healthy(),
            _ => singbox_dead(),
        })
    });
    let after: Vec<(u64, Action)> = actions(&log)
        .into_iter()
        .filter(|(t, _)| *t >= 32)
        .collect();
    let first = after.first().copied();
    assert_eq!(
        first,
        Some((confirmed_at(32), Action::RestartSingbox)),
        "relapse must re-confirm and restart from the ladder head"
    );
}

#[test]
fn flapping_fault_respects_action_cap_and_stable_recovery() {
    let log = Log::default();
    // Five failing cycles, one healthy, repeated: one good cycle must not
    // restart the ladder, and the sliding-window cap bounds total actions.
    drive(&log, 0, HOUR, |t| {
        Ok(if t % 6 == 5 { healthy() } else { blackhole() })
    });
    let n = actions(&log).len();
    assert!(
        n <= usize::from(CFG.action_cap),
        "actions {n} exceed cap {}",
        CFG.action_cap
    );
    assert!(n > 0, "expected at least one remedy before the cap");
    // Cap notice once when the window fills.
    let caps = log
        .notes
        .borrow()
        .iter()
        .filter(|(_, r)| *r == Reason::ActionCapReached)
        .count();
    assert_eq!(caps, 1, "cap notice must be one-shot");
}

#[test]
fn controller_restart_reconfirms_before_acting() {
    let log = Log::default();
    drive(&log, 0, 20, |_| Ok(singbox_dead()));
    let before = actions(&log).len();
    // A fresh process: state and cooldown are in memory only.
    drive(&log, 20, 20, |_| Ok(singbox_dead()));
    let after: Vec<(u64, Action)> = actions(&log).into_iter().skip(before).collect();
    assert_eq!(
        after.first().copied(),
        Some((confirmed_at(20), Action::RestartSingbox))
    );
}

#[test]
fn link_down_never_reboots_and_notifies_once() {
    let log = Log::default();
    drive(&log, 0, HOUR, |_| Ok(link_down()));
    assert_eq!(count(&log, Action::Reboot), 0, "no reboot for cable fault");
    assert_eq!(
        count(&log, Action::BounceWan),
        0,
        "no bounce without carrier"
    );
    assert_eq!(count(&log, Action::FailoverBackup), 1);
    let downs = log
        .notes
        .borrow()
        .iter()
        .filter(|(_, r)| *r == Reason::LinkDown)
        .count();
    assert_eq!(downs, 1, "link-down notice once, not every cycle");
}

#[test]
fn intermittent_probe_holds_confirmation() {
    let log = Log::default();
    // Confirm singbox death, then alternate probe timeout with death.
    // Unobserved cycles must not reset confirmation or emit Recovered.
    drive(&log, 0, 40, |t| match t {
        0..=10 => Ok(singbox_dead()),
        t if t % 2 == 0 => Err(ProbeError::Timeout),
        _ => Ok(singbox_dead()),
    });
    assert_eq!(recovered_notes(&log), 0);
    // First remedy still happened while probes worked.
    assert_eq!(count(&log, Action::RestartSingbox), 1);
}

#[test]
fn probe_dark_notifies_once_without_acting() {
    let log = Log::default();
    drive(&log, 0, 40, |_| Err(ProbeError::Timeout));
    assert!(actions(&log).is_empty());
    let dark = log
        .notes
        .borrow()
        .iter()
        .filter(|(_, r)| matches!(r, Reason::TelemetryDark { .. }))
        .count();
    assert_eq!(dark, 1);
}

#[test]
fn regression_before_policies_link_down_would_reboot() {
    // Documents the pre-policy behaviour the journal captured: with the old
    // ladder a cable-out climbed to Reboot. The live CFG must keep reboots at 0.
    let log = Log::default();
    drive(&log, 0, HOUR, |_| Ok(link_down()));
    assert_eq!(
        count(&log, Action::Reboot),
        0,
        "policy regression: cable-out must not reboot"
    );
}

#[test]
fn regression_before_policies_flapping_unbounded() {
    let log = Log::default();
    drive(&log, 0, HOUR, |t| {
        Ok(if t % 6 == 5 { healthy() } else { blackhole() })
    });
    assert!(
        actions(&log).len() <= usize::from(CFG.action_cap),
        "policy regression: flapping must be capped (was 20 BounceWan/h)"
    );
}

/// Prints what the current policy does across the sequences, including the
/// gaps that are not asserted. Read with `--nocapture`.
#[test]
fn scenario_journal() {
    type Gen = fn(u64) -> Sample;
    let cases: [(&str, Gen); 7] = [
        ("singbox dead, 1 h", |_| Ok(singbox_dead())),
        ("measured blackhole, 1 h", |_| Ok(blackhole())),
        (
            "PPPoE in discovery, link up, 1 h",
            |_| Ok(pppoe_discovery()),
        ),
        ("link down (no carrier), 1 h", |_| Ok(link_down())),
        ("blackhole flapping 5 bad / 1 good, 1 h", |t| {
            Ok(if t % 6 == 5 { healthy() } else { blackhole() })
        }),
        ("link down, probe fails every 3rd cycle, 1 h", |t| {
            if t % 3 == 2 {
                Err(ProbeError::Timeout)
            } else {
                Ok(link_down())
            }
        }),
        ("probe outage only, 1 h", |_| Err(ProbeError::Timeout)),
    ];
    println!(
        "scenario | actions | bounce | failover | restart | reboot | recovered | link_down | cap"
    );
    for (name, gen) in cases {
        let log = Log::default();
        drive(&log, 0, HOUR, gen);
        let link_n = log
            .notes
            .borrow()
            .iter()
            .filter(|(_, r)| *r == Reason::LinkDown)
            .count();
        let cap_n = log
            .notes
            .borrow()
            .iter()
            .filter(|(_, r)| *r == Reason::ActionCapReached)
            .count();
        println!(
            "{name} | {} | {} | {} | {} | {} | {} | {} | {}",
            actions(&log).len(),
            count(&log, Action::BounceWan),
            count(&log, Action::FailoverBackup),
            count(&log, Action::RestartSingbox),
            count(&log, Action::Reboot),
            recovered_notes(&log),
            link_n,
            cap_n,
        );
    }
}

#[test]
fn unobserved_cycle_is_never_announced_as_recovery() {
    let log = Log::default();
    // The link stays down; every third probe times out. Nothing was seen to
    // recover, so no "recovered" notice may go to a human. Confirmation held
    // across dark cycles may still produce one FailoverBackup.
    drive(&log, 0, HOUR, |t| {
        if t % 3 == 2 {
            Err(ProbeError::Timeout)
        } else {
            Ok(link_down())
        }
    });
    assert_eq!(recovered_notes(&log), 0);
    assert_eq!(count(&log, Action::Reboot), 0);
    assert!(count(&log, Action::FailoverBackup) <= 1);
}
