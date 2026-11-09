// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The supervisor: a pure finite-state machine.
//!
//! [`decide`] turns a stream of failure classifications into remediation
//! actions while obeying three disciplines that keep an automatic system from
//! doing more harm than the fault it is chasing:
//!
//! * **Hysteresis** — never react to a single blip; a fault must be confirmed
//!   `confirm_threshold` cycles running before any disruptive action.
//! * **Escalation** — always try the least-disruptive remedy first and climb a
//!   fixed, finite ladder only if the fault persists (bounded
//!   execution — the ladder terminates, so escalation cannot run away).
//! * **Cooldown + backoff** — after acting, wait before re-evaluating, and widen
//!   the wait as the level rises, so the supervisor can never thrash.
//!
//! `decide` is total, deterministic, and cannot panic: no unwrap, no indexing,
//! only saturating arithmetic.

use crate::domain::{LatencyMs, Telemetry, Tick};
use crate::failure::{classify, FailureClass};

/// Remediation actions, ordered from least to most disruptive. The supervisor
/// only ever climbs this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep watching; take no disruptive action.
    Observe,
    /// Ask the tunnel to select a different exit node (no restart).
    SwitchNode,
    /// Restart the tunnel process.
    RestartSingbox,
    /// Bounce the WAN interface (ifdown / ifup).
    BounceWan,
    /// Cut over to the backup uplink (mwan3 → USB tether).
    FailoverBackup,
    /// Reboot the device. Terminal remediation.
    Reboot,
}

/// Tunable thresholds. Passed explicitly on every call — the core holds no
/// configuration of its own (zero global state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Latency above which a `Healthy` tunnel is reclassified as degraded.
    pub degraded_above: LatencyMs,
    /// Consecutive confirmations of the same fault required before acting.
    pub confirm_threshold: u8,
    /// Base cooldown, in ticks, applied after an action (scaled by level).
    pub cooldown_ticks: u64,
    /// Consecutive *observed* healthy cycles required before escalation level
    /// resets. One good cycle among faults must not restart the ladder.
    pub recover_stable_ticks: u8,
    /// Max disruptive actions inside [`Self::action_cap_window`] ticks.
    /// `0` disables the cap.
    pub action_cap: u8,
    /// Sliding window length, in ticks, for [`Self::action_cap`]
    /// (default 120 ≈ 1 h at a 30 s interval).
    pub action_cap_window: u64,
    /// Consecutive failed probes before a one-shot "telemetry dark" notice.
    /// `0` disables the notice.
    pub unobserved_notify_after: u8,
}

impl Config {
    /// Classic three-field constructor with conservative safety defaults.
    #[must_use]
    pub const fn classic(
        degraded_above: LatencyMs,
        confirm_threshold: u8,
        cooldown_ticks: u64,
    ) -> Self {
        Self {
            degraded_above,
            confirm_threshold,
            cooldown_ticks,
            recover_stable_ticks: 3,
            action_cap: 6,
            action_cap_window: 120,
            unobserved_notify_after: 20,
        }
    }
}

/// Where to resume if a fault returns during a stabilizing window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StabilizeResume {
    /// Fault was never confirmed; keep the suspicion counter.
    Unconfirmed { seen: u8 },
    /// A remedy was already applied at this escalation level.
    Confirmed { level: u8 },
}

/// The supervisor's memory between cycles. Every reachable value is valid by
/// construction (make invalid states unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Everything nominal.
    Healthy,
    /// A fault has been seen `seen` times but not yet confirmed.
    Suspect { failing: FailureClass, seen: u8 },
    /// An action was just emitted; hold until `until`, then re-evaluate.
    Cooldown {
        failing: FailureClass,
        level: u8,
        until: Tick,
    },
    /// Healthy samples after a fault: keep the prior escalation level until
    /// `seen_ok` reaches `recover_stable_ticks`.
    Stabilizing {
        failing: FailureClass,
        resume: StabilizeResume,
        seen_ok: u8,
    },
}

/// Why a transition happened — a machine-readable, allocation-free explanation
/// suitable for logging with full context (errors and decisions are
/// never silent). Adapters render it into human text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// System is healthy; nothing to do.
    Nominal,
    /// A fault was seen for the first time; watching before reacting.
    FirstSuspicion(FailureClass),
    /// The same fault persists but is not yet confirmed.
    AwaitingConfirmation { failing: FailureClass, seen: u8 },
    /// The fault cleared before it was confirmed or during cooldown.
    Recovered,
    /// A fault was confirmed and the first remedy is being applied.
    Confirmed { failing: FailureClass, level: u8 },
    /// Waiting out the cooldown window.
    CoolingDown,
    /// The fault outlived a remedy; climbing to a stronger one.
    Escalated { failing: FailureClass, level: u8 },
    /// Counting consecutive healthy cycles before clearing escalation.
    Stabilizing { seen: u8 },
    /// Physical WAN carrier is down; ladder stopped at failover (no reboot).
    LinkDown,
    /// The sliding-window action cap was hit; holding disruptive remedies.
    ActionCapReached,
    /// Probes have failed this many cycles in a row; state is frozen.
    TelemetryDark { cycles: u8 },
}

/// The output of one supervision cycle: where the machine goes next, what the
/// caller should do, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub next: State,
    pub action: Action,
    pub reason: Reason,
}

/// The remediation ladder for a fault: least-disruptive first, terminal action
/// last. Returning a fixed slice makes the escalation space finite and auditable.
const fn ladder(class: FailureClass) -> &'static [Action] {
    match class {
        FailureClass::None => &[Action::Observe],
        FailureClass::NodeDegraded => &[Action::SwitchNode, Action::RestartSingbox],
        // A dead/unreachable node is a tunnel-layer problem: try another node,
        // then restart the tunnel. Bouncing WAN for it is inappropriate (that is
        // for an actual WAN failure) and was observed firing wrongly on transient
        // probe timeouts, so it is deliberately excluded.
        FailureClass::NodeDead => &[Action::SwitchNode, Action::RestartSingbox],
        FailureClass::SingboxDead => &[Action::RestartSingbox, Action::Reboot],
        FailureClass::DnsFail => &[Action::RestartSingbox, Action::Reboot],
        FailureClass::WanDown => &[Action::BounceWan, Action::FailoverBackup, Action::Reboot],
    }
}

/// Cable unplugged / no carrier: match on-router `wan-selfheal`, which refuses
/// to bounce when carrier is down. Fail over once; never reboot for L1.
const CABLE_LADDER: &[Action] = &[Action::FailoverBackup];

const fn ladder_for(class: FailureClass, link_down: bool) -> &'static [Action] {
    if link_down && matches!(class, FailureClass::WanDown) {
        CABLE_LADDER
    } else {
        ladder(class)
    }
}

/// The action for a fault at a given escalation level. Levels past the end of the
/// ladder saturate at its terminal action, so an unexpected level can never index
/// out of bounds nor invent an action.
fn action_at(class: FailureClass, level: u8, link_down: bool) -> Action {
    let steps = ladder_for(class, link_down);
    let last = steps.len().saturating_sub(1);
    let idx = usize::from(level).min(last);
    steps.get(idx).copied().unwrap_or(Action::Reboot)
}

fn link_is_down(tel: &Telemetry) -> bool {
    matches!(tel.aux.link, crate::domain::Link::Down)
}

/// Run one supervision cycle. Pure: identical inputs always yield identical output.
#[must_use]
pub fn decide(state: State, tel: &Telemetry, cfg: &Config, now: Tick) -> Transition {
    let class = classify(tel, cfg.degraded_above);
    let link_down = link_is_down(tel);
    match state {
        State::Healthy => on_healthy(class),
        State::Suspect { failing, seen } => on_suspect(failing, seen, class, cfg, now, link_down),
        State::Cooldown {
            failing,
            level,
            until,
        } => on_cooldown(failing, level, until, class, cfg, now, link_down),
        State::Stabilizing {
            failing,
            resume,
            seen_ok,
        } => on_stabilizing(failing, resume, seen_ok, class, cfg, now, link_down),
    }
}

fn on_healthy(class: FailureClass) -> Transition {
    match class {
        FailureClass::None => Transition {
            next: State::Healthy,
            action: Action::Observe,
            reason: Reason::Nominal,
        },
        fault => Transition {
            // Hysteresis: record the suspicion but do not act on first sight.
            next: State::Suspect {
                failing: fault,
                seen: 1,
            },
            action: Action::Observe,
            reason: Reason::FirstSuspicion(fault),
        },
    }
}

fn begin_stabilizing(failing: FailureClass, resume: StabilizeResume, cfg: &Config) -> Transition {
    // recover_stable_ticks <= 1 preserves the legacy "one healthy cycle clears".
    if cfg.recover_stable_ticks <= 1 {
        return Transition {
            next: State::Healthy,
            action: Action::Observe,
            reason: Reason::Recovered,
        };
    }
    Transition {
        next: State::Stabilizing {
            failing,
            resume,
            seen_ok: 1,
        },
        action: Action::Observe,
        reason: Reason::Stabilizing { seen: 1 },
    }
}

fn on_suspect(
    failing: FailureClass,
    seen: u8,
    class: FailureClass,
    cfg: &Config,
    now: Tick,
    link_down: bool,
) -> Transition {
    if class == FailureClass::None {
        // Confirmation requires consecutive faults; a healthy cycle clears the
        // suspicion entirely. Escalation level is preserved only after a
        // confirmed remedy (Cooldown → Stabilizing).
        return Transition {
            next: State::Healthy,
            action: Action::Observe,
            reason: Reason::Recovered,
        };
    }
    if class != failing {
        // The fault changed identity; confirmation restarts for the new one.
        return Transition {
            next: State::Suspect {
                failing: class,
                seen: 1,
            },
            action: Action::Observe,
            reason: Reason::FirstSuspicion(class),
        };
    }
    let seen = seen.saturating_add(1);
    if seen >= cfg.confirm_threshold {
        Transition {
            // Confirmed: emit the first remedy and hold a real cooldown so the
            // next cycle cannot immediately escalate.
            next: State::Cooldown {
                failing: class,
                level: 0,
                until: now.saturating_add(cfg.cooldown_ticks),
            },
            action: action_at(class, 0, link_down),
            reason: if link_down && class == FailureClass::WanDown {
                Reason::LinkDown
            } else {
                Reason::Confirmed {
                    failing: class,
                    level: 0,
                }
            },
        }
    } else {
        Transition {
            next: State::Suspect {
                failing: class,
                seen,
            },
            action: Action::Observe,
            reason: Reason::AwaitingConfirmation {
                failing: class,
                seen,
            },
        }
    }
}

fn on_cooldown(
    failing: FailureClass,
    level: u8,
    until: Tick,
    class: FailureClass,
    cfg: &Config,
    now: Tick,
    link_down: bool,
) -> Transition {
    // Recovery starts a stabilizing window; one healthy cycle must not clear
    // the escalation level.
    if class == FailureClass::None {
        return begin_stabilizing(failing, StabilizeResume::Confirmed { level }, cfg);
    }
    if now < until {
        return Transition {
            next: State::Cooldown {
                failing,
                level,
                until,
            },
            action: Action::Observe,
            reason: Reason::CoolingDown,
        };
    }
    // Cooldown elapsed and a fault persists. If it is the same fault, climb one
    // rung; if it changed, treat it as a freshly confirmed fault at level 0.
    let (next_class, next_level) = if class == failing {
        (failing, level.saturating_add(1))
    } else {
        (class, 0)
    };
    let steps = ladder_for(next_class, link_down);
    let terminal = u8::try_from(steps.len().saturating_sub(1)).unwrap_or(0);
    // Cable fault: after the single FailoverBackup, hold — never re-fire and
    // never climb to Reboot (consistent with wan-selfheal on no carrier).
    if link_down && next_class == FailureClass::WanDown && level >= terminal && class == failing {
        let span = cfg
            .cooldown_ticks
            .saturating_mul(u64::from(level).saturating_add(1));
        return Transition {
            next: State::Cooldown {
                failing,
                level,
                until: now.saturating_add(span),
            },
            action: Action::Observe,
            reason: Reason::CoolingDown,
        };
    }
    // Backoff widens with level so repeated remedies cannot thrash. Bounded: the
    // multiplier is (level + 1) and the ladder itself is finite.
    let span = cfg
        .cooldown_ticks
        .saturating_mul(u64::from(next_level).saturating_add(1));
    Transition {
        next: State::Cooldown {
            failing: next_class,
            level: next_level,
            until: now.saturating_add(span),
        },
        action: action_at(next_class, next_level, link_down),
        reason: Reason::Escalated {
            failing: next_class,
            level: next_level,
        },
    }
}

fn on_stabilizing(
    failing: FailureClass,
    resume: StabilizeResume,
    seen_ok: u8,
    class: FailureClass,
    cfg: &Config,
    now: Tick,
    _link_down: bool,
) -> Transition {
    if class == FailureClass::None {
        let seen_ok = seen_ok.saturating_add(1);
        let need = cfg.recover_stable_ticks.max(1);
        if seen_ok >= need {
            return Transition {
                next: State::Healthy,
                action: Action::Observe,
                reason: Reason::Recovered,
            };
        }
        return Transition {
            next: State::Stabilizing {
                failing,
                resume,
                seen_ok,
            },
            action: Action::Observe,
            reason: Reason::Stabilizing { seen: seen_ok },
        };
    }
    if class != failing {
        return Transition {
            next: State::Suspect {
                failing: class,
                seen: 1,
            },
            action: Action::Observe,
            reason: Reason::FirstSuspicion(class),
        };
    }
    match resume {
        StabilizeResume::Unconfirmed { .. } => Transition {
            // Unconfirmed stabilizing is unused (suspect clears immediately);
            // restart confirmation if we ever land here.
            next: State::Suspect { failing, seen: 1 },
            action: Action::Observe,
            reason: Reason::FirstSuspicion(failing),
        },
        StabilizeResume::Confirmed { level } => Transition {
            next: State::Cooldown {
                failing,
                level,
                until: now.saturating_add(cfg.cooldown_ticks),
            },
            action: Action::Observe,
            reason: Reason::CoolingDown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, Action, Config, Reason, State, Transition};
    use crate::domain::{Aux, Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use crate::failure::{classify, FailureClass};

    fn cfg() -> Config {
        Config {
            degraded_above: LatencyMs(300),
            confirm_threshold: 3,
            cooldown_ticks: 5,
            recover_stable_ticks: 1,
            action_cap: 0,
            action_cap_window: 120,
            unobserved_notify_after: 0,
        }
    }

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

    #[test]
    fn healthy_stays_healthy() {
        let Transition {
            next,
            action,
            reason,
        } = decide(State::Healthy, &healthy(), &cfg(), Tick(0));
        assert_eq!(next, State::Healthy);
        assert_eq!(action, Action::Observe);
        assert_eq!(reason, Reason::Nominal);
    }

    #[test]
    fn first_fault_only_watches() {
        let mut tel = healthy();
        tel.singbox = Proc::Dead;
        let t = decide(State::Healthy, &tel, &cfg(), Tick(0));
        assert_eq!(
            t.next,
            State::Suspect {
                failing: FailureClass::SingboxDead,
                seen: 1
            }
        );
        assert_eq!(t.action, Action::Observe, "must never act on first sight");
    }

    #[test]
    fn confirmed_fault_emits_first_remedy() {
        let mut tel = healthy();
        tel.singbox = Proc::Dead;
        // seen == 2, threshold == 3, so this cycle confirms.
        let s = State::Suspect {
            failing: FailureClass::SingboxDead,
            seen: 2,
        };
        let t = decide(s, &tel, &cfg(), Tick(10));
        assert_eq!(t.action, Action::RestartSingbox);
        // Cooldown deadline must be now + cooldown_ticks (10 + 5), not zero.
        assert!(matches!(
            t.next,
            State::Cooldown { failing: FailureClass::SingboxDead, level: 0, until }
                if until == Tick(15)
        ));
    }

    #[test]
    fn recovery_before_confirmation_returns_to_healthy() {
        let s = State::Suspect {
            failing: FailureClass::SingboxDead,
            seen: 2,
        };
        let t = decide(s, &healthy(), &cfg(), Tick(5));
        assert_eq!(t.next, State::Healthy);
        assert_eq!(t.reason, Reason::Recovered);
    }

    #[test]
    fn cooldown_holds_until_deadline() {
        let s = State::Cooldown {
            failing: FailureClass::WanDown,
            level: 0,
            until: Tick(20),
        };
        let mut tel = healthy();
        tel.wan = Wan::Down;
        let t = decide(s, &tel, &cfg(), Tick(10));
        assert_eq!(t.action, Action::Observe);
        assert_eq!(t.reason, Reason::CoolingDown);
    }

    #[test]
    fn persistent_fault_escalates_after_cooldown() {
        let s = State::Cooldown {
            failing: FailureClass::WanDown,
            level: 0,
            until: Tick(20),
        };
        let mut tel = healthy();
        tel.wan = Wan::Down;
        let t = decide(s, &tel, &cfg(), Tick(20));
        // WanDown ladder: [BounceWan, FailoverBackup, Reboot]; level 1 -> FailoverBackup.
        assert_eq!(t.action, Action::FailoverBackup);
        assert!(matches!(t.next, State::Cooldown { level: 1, .. }));
    }

    #[test]
    fn recovery_during_cooldown_returns_to_healthy() {
        let s = State::Cooldown {
            failing: FailureClass::WanDown,
            level: 2,
            until: Tick(20),
        };
        let t = decide(s, &healthy(), &cfg(), Tick(5));
        assert_eq!(t.next, State::Healthy);
        assert_eq!(t.reason, Reason::Recovered);
    }

    #[test]
    fn wan_down_outranks_shallower_faults() {
        let mut tel = healthy();
        tel.wan = Wan::Down;
        tel.singbox = Proc::Dead;
        tel.dns = Dns::Failing;
        assert_eq!(classify(&tel, LatencyMs(300)), FailureClass::WanDown);
    }

    #[test]
    fn high_latency_is_degraded() {
        let mut tel = healthy();
        tel.tunnel = Tunnel::Healthy {
            latency: LatencyMs(500),
        };
        assert_eq!(classify(&tel, LatencyMs(300)), FailureClass::NodeDegraded);
    }

    #[test]
    fn link_down_stops_at_failover_never_reboots() {
        use crate::domain::{Aux, Link, WanReach};
        let mut tel = healthy();
        tel.wan = Wan::Down;
        tel.aux = Aux {
            link: Link::Down,
            wan_reach: WanReach::Blackhole,
            ..Aux::UNKNOWN
        };
        let s = State::Suspect {
            failing: FailureClass::WanDown,
            seen: 2,
        };
        let t = decide(s, &tel, &cfg(), Tick(10));
        assert_eq!(t.action, Action::FailoverBackup);
        // After the only rung, further escalations observe only.
        assert!(
            matches!(t.next, State::Cooldown { .. }),
            "expected cooldown after failover"
        );
        if let State::Cooldown { level, until, .. } = t.next {
            let t2 = decide(
                State::Cooldown {
                    failing: FailureClass::WanDown,
                    level,
                    until,
                },
                &tel,
                &cfg(),
                until,
            );
            assert_eq!(t2.action, Action::Observe);
            assert_ne!(t2.action, Action::Reboot);
            assert_ne!(t2.action, Action::BounceWan);
        }
    }

    #[test]
    fn one_healthy_cycle_does_not_clear_escalation_level() {
        let mut bad = healthy();
        bad.wan = Wan::Down;
        let mut cfg = cfg();
        cfg.recover_stable_ticks = 3;
        let cool = State::Cooldown {
            failing: FailureClass::WanDown,
            level: 1,
            until: Tick(20),
        };
        let t = decide(cool, &healthy(), &cfg, Tick(5));
        assert!(matches!(
            t.next,
            State::Stabilizing {
                failing: FailureClass::WanDown,
                resume: super::StabilizeResume::Confirmed { level: 1 },
                seen_ok: 1
            }
        ));
        let t2 = decide(t.next, &bad, &cfg, Tick(6));
        assert!(matches!(
            t2.next,
            State::Cooldown {
                failing: FailureClass::WanDown,
                level: 1,
                ..
            }
        ));
    }
}
