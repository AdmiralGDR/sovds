//! The audit / training record: a complete, self-describing snapshot of one
//! supervision cycle, and its sovereign serialization.
//!
//! One [`Record`] is both the runtime report of a cycle *and* the durable datum a
//! future model will train on — one type, one source of truth. The format is
//! defined here, in the core, so it is sovereign and stable for the long haul
//! (longevity): a single line of `key=value` fields that any tool — `grep`, a
//! parser we write later, a human in ten years — can read.
//!
//! Serialization writes into any [`core::fmt::Write`] sink, so it needs no
//! allocation and stays `no_std`: the caller supplies the buffer (a heapless one
//! on-device, a `String` in the file adapter). The record schema itself never
//! touches I/O (the principle).

use crate::domain::{Dns, Proc, Telemetry, Tick, Tunnel, Wan};
use crate::failure::FailureClass;
use crate::fleet::NodeVerdict;
use crate::fsm::{Action, Reason, State};
use core::fmt::Write;

/// What became of the action taken this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No action was taken (the decision was to observe).
    NotActed,
    /// An action was applied and its post-condition verified.
    Verified,
    /// An action was attempted but could not be verified.
    Unverified,
}

/// A complete record of one supervision cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub tick: Tick,
    /// Whether the router could be observed this cycle (`false` == ran degraded).
    pub probed: bool,
    /// The raw observation from the router, before adaptive refinement — the
    /// ground truth a model learns from.
    pub telemetry: Telemetry,
    /// The adaptive verdict on the active path this cycle.
    pub path_verdict: NodeVerdict,
    /// The fault the (refined) observation classified to.
    pub failure: FailureClass,
    /// The state the machine moved into.
    pub state: State,
    pub reason: Reason,
    pub action: Action,
    pub outcome: Outcome,
}

/// Serialize a record as one self-describing line (no trailing newline). The
/// output is deterministic and allocation-free.
///
/// # Errors
/// Propagates any error from the underlying sink `w`.
pub fn write_record<W: Write>(w: &mut W, r: &Record) -> core::fmt::Result {
    write!(w, "t={}", r.tick.0)?;
    write!(w, " probed={}", u8::from(r.probed))?;
    w.write_str(" wan=")?;
    write_wan(w, r.telemetry.wan)?;
    w.write_str(" sb=")?;
    w.write_str(proc_label(r.telemetry.singbox))?;
    w.write_str(" tun=")?;
    write_tunnel(w, r.telemetry.tunnel)?;
    w.write_str(" dns=")?;
    w.write_str(dns_label(r.telemetry.dns))?;
    w.write_str(" verdict=")?;
    w.write_str(verdict_label(r.path_verdict))?;
    w.write_str(" class=")?;
    w.write_str(failure_label(r.failure))?;
    w.write_str(" state=")?;
    write_state(w, r.state)?;
    w.write_str(" reason=")?;
    w.write_str(reason_label(r.reason))?;
    w.write_str(" action=")?;
    w.write_str(action_label(r.action))?;
    w.write_str(" outcome=")?;
    w.write_str(outcome_label(r.outcome))
}

fn write_wan<W: Write>(w: &mut W, wan: Wan) -> core::fmt::Result {
    match wan {
        Wan::Up { uptime_s } => write!(w, "up:{uptime_s}"),
        Wan::Down => w.write_str("down"),
        Wan::Unknown => w.write_str("unknown"),
    }
}

fn write_tunnel<W: Write>(w: &mut W, tunnel: Tunnel) -> core::fmt::Result {
    match tunnel {
        Tunnel::Healthy { latency } => write!(w, "ok:{}", latency.0),
        Tunnel::Degraded { latency } => write!(w, "deg:{}", latency.0),
        Tunnel::Dead => w.write_str("dead"),
        Tunnel::Unknown => w.write_str("unknown"),
    }
}

fn write_state<W: Write>(w: &mut W, state: State) -> core::fmt::Result {
    match state {
        State::Healthy => w.write_str("healthy"),
        State::Suspect { failing, seen } => {
            write!(w, "suspect:{}:{}", failure_label(failing), seen)
        }
        State::Cooldown {
            failing,
            level,
            until,
        } => {
            write!(
                w,
                "cooldown:{}:{}:{}",
                failure_label(failing),
                level,
                until.0
            )
        }
    }
}

const fn proc_label(p: Proc) -> &'static str {
    match p {
        Proc::Running => "running",
        Proc::Dead => "dead",
        Proc::Unknown => "unknown",
    }
}

const fn dns_label(d: Dns) -> &'static str {
    match d {
        Dns::Ok => "ok",
        Dns::Failing => "failing",
        Dns::Unknown => "unknown",
    }
}

const fn verdict_label(v: NodeVerdict) -> &'static str {
    match v {
        NodeVerdict::Warming => "warming",
        NodeVerdict::Healthy => "healthy",
        NodeVerdict::Slow => "slow",
        NodeVerdict::Throttled => "throttled",
        NodeVerdict::Down => "down",
    }
}

const fn failure_label(f: FailureClass) -> &'static str {
    match f {
        FailureClass::None => "none",
        FailureClass::WanDown => "wan_down",
        FailureClass::SingboxDead => "singbox_dead",
        FailureClass::NodeDead => "node_dead",
        FailureClass::NodeDegraded => "node_degraded",
        FailureClass::DnsFail => "dns_fail",
    }
}

const fn reason_label(r: Reason) -> &'static str {
    match r {
        Reason::Nominal => "nominal",
        Reason::FirstSuspicion(_) => "first_suspicion",
        Reason::AwaitingConfirmation { .. } => "awaiting",
        Reason::Recovered => "recovered",
        Reason::Confirmed { .. } => "confirmed",
        Reason::CoolingDown => "cooling",
        Reason::Escalated { .. } => "escalated",
    }
}

const fn action_label(a: Action) -> &'static str {
    match a {
        Action::Observe => "observe",
        Action::SwitchNode => "switch_node",
        Action::RestartSingbox => "restart_singbox",
        Action::BounceWan => "bounce_wan",
        Action::FailoverBackup => "failover_backup",
        Action::Reboot => "reboot",
    }
}

const fn outcome_label(o: Outcome) -> &'static str {
    match o {
        Outcome::NotActed => "-",
        Outcome::Verified => "verified",
        Outcome::Unverified => "unverified",
    }
}

#[cfg(test)]
mod tests {
    use super::{write_record, Outcome, Record};
    use crate::domain::{Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use crate::failure::FailureClass;
    use crate::fleet::NodeVerdict;
    use crate::fsm::{Action, Reason, State};

    fn sample() -> Record {
        Record {
            tick: Tick(42),
            probed: true,
            telemetry: Telemetry {
                wan: Wan::Up { uptime_s: 12345 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(439),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
            },
            path_verdict: NodeVerdict::Healthy,
            failure: FailureClass::None,
            state: State::Healthy,
            reason: Reason::Nominal,
            action: Action::Observe,
            outcome: Outcome::NotActed,
        }
    }

    #[test]
    fn serializes_a_single_self_describing_line() -> core::fmt::Result {
        let mut line = String::new();
        write_record(&mut line, &sample())?;
        assert!(!line.contains('\n'), "a record must be exactly one line");
        for token in [
            "t=42",
            "probed=1",
            "wan=up:12345",
            "sb=running",
            "tun=ok:439",
            "dns=ok",
        ] {
            assert!(line.contains(token), "missing {token} in: {line}");
        }
        for token in [
            "verdict=healthy",
            "class=none",
            "state=healthy",
            "action=observe",
        ] {
            assert!(line.contains(token), "missing {token} in: {line}");
        }
        Ok(())
    }

    #[test]
    fn serializes_a_fault_cycle() -> core::fmt::Result {
        let mut r = sample();
        r.telemetry.singbox = Proc::Dead;
        r.failure = FailureClass::SingboxDead;
        r.state = State::Suspect {
            failing: FailureClass::SingboxDead,
            seen: 2,
        };
        r.reason = Reason::Confirmed {
            failing: FailureClass::SingboxDead,
            level: 0,
        };
        r.action = Action::RestartSingbox;
        r.outcome = Outcome::Verified;
        let mut line = String::new();
        write_record(&mut line, &r)?;
        assert!(line.contains("sb=dead"));
        assert!(line.contains("class=singbox_dead"));
        assert!(line.contains("state=suspect:singbox_dead:2"));
        assert!(line.contains("action=restart_singbox"));
        assert!(line.contains("outcome=verified"));
        Ok(())
    }
}
