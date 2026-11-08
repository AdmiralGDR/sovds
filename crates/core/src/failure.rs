// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Failure classification.
//!
//! [`classify`] collapses a raw [`Telemetry`] observation into the single most
//! urgent fault. It is pure and total: every possible observation maps to
//! exactly one [`FailureClass`].

use crate::domain::{Dns, LatencyMs, Proc, Telemetry, Tunnel, Wan, WanReach};

/// The kind of fault detected this cycle. `None` means the system looks healthy.
///
/// Exactly one class is reported, chosen by causal depth (WAN → process → path →
/// name resolution), because repairing a deeper layer usually clears the
/// shallower symptoms it causes — a dead WAN also makes the tunnel look dead, so
/// there is no point reacting to the tunnel first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// No fault detected.
    None,
    /// WAN link is down — the most fundamental failure; nothing upstream works.
    WanDown,
    /// The tunnel process is not running.
    SingboxDead,
    /// The selected node carries no traffic though process and WAN are fine.
    NodeDead,
    /// The selected node works but is slower than the healthy budget.
    NodeDegraded,
    /// Name resolution is failing though the tunnel path itself is up.
    DnsFail,
}

/// Classify one observation.
///
/// Precedence follows causal depth so the supervisor always attacks the root
/// layer first. `Unknown` readings are treated as "not a failure" here: a single
/// missed probe must never, by itself, provoke a disruptive action — that job
/// belongs to hysteresis in the state machine.
#[must_use]
pub fn classify(tel: &Telemetry, degraded_above: LatencyMs) -> FailureClass {
    match tel.wan {
        Wan::Down => return FailureClass::WanDown,
        // A live session that carries no L3 is the "WAN LED on, no internet"
        // failure. Only a *measured* blackhole counts — omitted/unknown
        // `wanok` must not invent this class (old records, failed sub-probes).
        Wan::Up { .. } if tel.aux.wan_reach == WanReach::Blackhole => return FailureClass::WanDown,
        Wan::Up { .. } | Wan::Unknown => {}
    }
    match tel.singbox {
        Proc::Dead => return FailureClass::SingboxDead,
        Proc::Running | Proc::Unknown => {}
    }
    match tel.tunnel {
        Tunnel::Dead => return FailureClass::NodeDead,
        Tunnel::Degraded { .. } => return FailureClass::NodeDegraded,
        Tunnel::Healthy { latency } if latency > degraded_above => {
            return FailureClass::NodeDegraded
        }
        Tunnel::Healthy { .. } | Tunnel::Unknown => {}
    }
    match tel.dns {
        Dns::Failing => FailureClass::DnsFail,
        Dns::Ok | Dns::Unknown => FailureClass::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, FailureClass};
    use crate::domain::{
        Aux, Dns, LatencyMs, Link, PppPhase, Proc, Telemetry, Tunnel, Wan, WanReach, MAX_NODES,
    };

    fn path(latency_ms: u32) -> Telemetry {
        Telemetry {
            wan: Wan::Up { uptime_s: 1 },
            singbox: Proc::Running,
            tunnel: Tunnel::Healthy {
                latency: LatencyMs(latency_ms),
            },
            dns: Dns::Ok,
            nodes: [None; MAX_NODES],
            aux: Aux::UNKNOWN,
        }
    }

    #[test]
    fn static_budget_must_clear_the_slower_exit_median() {
        // A slower exit whose ordinary RTT is 817 ms must not be labelled
        // node_degraded every cycle.
        assert_eq!(
            classify(&path(817), LatencyMs(800)),
            FailureClass::NodeDegraded
        );
        assert_eq!(classify(&path(817), LatencyMs(1500)), FailureClass::None);
    }

    #[test]
    fn unknown_aux_does_not_change_classify() {
        let mut t = path(200);
        t.aux.link = Link::Down;
        t.aux.ppp = PppPhase::Discovery;
        assert_eq!(t.aux.wan_reach, WanReach::Unknown);
        assert_eq!(classify(&t, LatencyMs(800)), FailureClass::None);
    }

    #[test]
    fn measured_blackhole_with_session_up_is_wan_down() {
        let mut t = path(200);
        t.aux.wan_reach = WanReach::Blackhole;
        assert_eq!(classify(&t, LatencyMs(800)), FailureClass::WanDown);
    }

    #[test]
    fn blackhole_does_not_outrank_an_already_down_session() {
        let mut t = path(200);
        t.wan = Wan::Down;
        t.aux.wan_reach = WanReach::Blackhole;
        assert_eq!(classify(&t, LatencyMs(800)), FailureClass::WanDown);
    }

    #[test]
    fn blackhole_outranks_a_stale_healthy_tunnel() {
        let mut t = path(200);
        t.aux.wan_reach = WanReach::Blackhole;
        t.singbox = Proc::Dead;
        t.dns = Dns::Failing;
        assert_eq!(classify(&t, LatencyMs(800)), FailureClass::WanDown);
    }
}
