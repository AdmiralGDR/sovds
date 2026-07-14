//! The supervisor: a pure finite-state machine.
//!
//! [`decide`] turns a stream of failure classifications into remediation
//! actions while obeying three disciplines that keep an automatic system from
//! doing more harm than the fault it is chasing:
//!
//! * **Hysteresis** — never react to a single blip; a fault must be confirmed
//!   `confirm_threshold` cycles running before any disruptive action.
//! * **Escalation** — always try the least-disruptive remedy first and climb a
//!   fixed, finite ladder only if the fault persists (the principle: bounded
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
/// configuration of its own (the principle: zero global state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Latency above which a `Healthy` tunnel is reclassified as degraded.
    pub degraded_above: LatencyMs,
    /// Consecutive confirmations of the same fault required before acting.
    pub confirm_threshold: u8,
    /// Base cooldown, in ticks, applied after an action (scaled by level).
    pub cooldown_ticks: u64,
}

/// The supervisor's memory between cycles. Every reachable value is valid by
/// construction (the principle: make invalid states unrepresentable).
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
}

/// Why a transition happened — a machine-readable, allocation-free explanation
/// suitable for logging with full context (the principle: errors and decisions are
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

/// The action for a fault at a given escalation level. Levels past the end of the
/// ladder saturate at its terminal action, so an unexpected level can never index
/// out of bounds (the principle) nor invent an action.
fn action_at(class: FailureClass, level: u8) -> Action {
    let steps = ladder(class);
    let last = steps.len().saturating_sub(1);
    let idx = usize::from(level).min(last);
    steps.get(idx).copied().unwrap_or(Action::Reboot)
}

/// Run one supervision cycle. Pure: identical inputs always yield identical output.
#[must_use]
pub fn decide(state: State, tel: &Telemetry, cfg: &Config, now: Tick) -> Transition {
    let class = classify(tel, cfg.degraded_above);
    match state {
        State::Healthy => on_healthy(class),
        State::Suspect { failing, seen } => on_suspect(failing, seen, class, cfg, now),
        State::Cooldown {
            failing,
            level,
            until,
        } => on_cooldown(failing, level, until, class, cfg, now),
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

fn on_suspect(
    failing: FailureClass,
    seen: u8,
    class: FailureClass,
    cfg: &Config,
    now: Tick,
) -> Transition {
    if class == FailureClass::None {
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
            action: action_at(class, 0),
            reason: Reason::Confirmed {
                failing: class,
                level: 0,
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
) -> Transition {
    // Recovery is honored immediately, even mid-cooldown.
    if class == FailureClass::None {
        return Transition {
            next: State::Healthy,
            action: Action::Observe,
            reason: Reason::Recovered,
        };
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
        action: action_at(next_class, next_level),
        reason: Reason::Escalated {
            failing: next_class,
            level: next_level,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, Action, Config, Reason, State, Transition};
    use crate::domain::{Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use crate::failure::{classify, FailureClass};

    fn cfg() -> Config {
        Config {
            degraded_above: LatencyMs(300),
            confirm_threshold: 3,
            cooldown_ticks: 5,
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
}
