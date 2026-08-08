// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Failure classification.
//!
//! [`classify`] collapses a raw [`Telemetry`] observation into the single most
//! urgent fault. It is pure and total: every possible observation maps to
//! exactly one [`FailureClass`].

use crate::domain::{Dns, LatencyMs, Proc, Telemetry, Tunnel, Wan};

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
