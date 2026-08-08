// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Telemetry adapter: gather one observation of the router and validate it at
//! the boundary before it reaches the sovereign core.
//!
//! The router runs [`REMOTE_TELEMETRY_SCRIPT`], which emits a tiny, line-based
//! `key=value` contract. [`parse_snapshot`] is the boundary guard (//! all external input is hostile): it accepts only well-formed values and turns
//! everything else into [`ProbeError::Malformed`]. It is pure, so the whole
//! parsing surface is unit-testable without a router.
//!
//! Judgement stays in the core: the router only *measures* latency (`tunnel=ok:N`)
//! and never decides whether that counts as "degraded" — that threshold lives in
//! the core's config.

use crate::os::{CommandRunner, RunError};
use crate::ssh::SshExec;
use sovereign_core::domain::{
    Aux, Dns, LatencyMs, NodeHealth, NodeId, Proc, Telemetry, Tunnel, Wan, WanReach, MAX_NODES,
};
use sovereign_core::error::ProbeError;
use sovereign_core::failure::FailureClass;
use sovereign_core::ports::{Observation, TelemetrySource};

/// The contract executed on the router. Each probe is isolated: if one fails, it
/// reports `unknown`/`dead` for its own key rather than failing the whole
/// snapshot (graceful degradation at the source). The Clash secret is
/// read from the router's own config so it never travels in this string.
///
/// This is the **single source** of the telemetry contract (no duplicated
/// logic) — the exact same `deploy/router/telemetry.sh` the router-side
/// `sovereign-probe` sources into the fallback buffer, embedded here at compile
/// time so the collector and the probe can never diverge.
pub const REMOTE_TELEMETRY_SCRIPT: &str = include_str!("../../../deploy/router/telemetry.sh");

/// Parse the router's telemetry contract into a validated [`Telemetry`].
///
/// The four core signals (`wan`, `singbox`, `tunnel`, `dns`) are mandatory; a
/// missing or unrecognised one is a hostile/garbled input and is rejected.
/// `node=<id>:<latency>` lines are optional and bounded by [`MAX_NODES`];
/// auxiliary keys (`wanok`, `load`, `mem`, `active`) are optional too — a probe
/// that failed to measure them simply omits them, and they parse to "unknown".
pub fn parse_snapshot(text: &str) -> Result<Observation, ProbeError> {
    let mut wan: Option<Wan> = None;
    let mut singbox: Option<Proc> = None;
    let mut tunnel: Option<Tunnel> = None;
    let mut dns: Option<Dns> = None;
    let mut nodes: [Option<NodeHealth>; MAX_NODES] = [None; MAX_NODES];
    let mut node_slot: usize = 0;
    let mut aux = Aux::UNKNOWN;
    let mut injected: Option<FailureClass> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(ProbeError::Malformed)?;
        match key {
            "wan" => wan = Some(parse_wan(value)?),
            "singbox" => singbox = Some(parse_proc(value)?),
            "tunnel" => tunnel = Some(parse_tunnel(value)?),
            "dns" => dns = Some(parse_dns(value)?),
            "node" => {
                let health = parse_node(value)?;
                if let Some(cell) = nodes.get_mut(node_slot) {
                    *cell = Some(health);
                    node_slot = node_slot.saturating_add(1);
                }
                // Nodes beyond MAX_NODES are dropped: the core reasons about a
                // bounded set, and extra measurements are not fatal.
            }
            "wanok" => {
                aux.wan_reach = match value {
                    "1" => WanReach::Ok,
                    "0" => WanReach::Blackhole,
                    _ => return Err(ProbeError::Malformed),
                }
            }
            "load" => {
                aux.load_centi = Some(value.parse::<u32>().map_err(|_| ProbeError::Malformed)?)
            }
            "mem" => {
                aux.mem_free_kb = Some(value.parse::<u32>().map_err(|_| ProbeError::Malformed)?)
            }
            "active" => aux.active = Some(value.parse::<u8>().map_err(|_| ProbeError::Malformed)?),
            "inject" => injected = Some(parse_failure_class(value)?),
            _ => return Err(ProbeError::Malformed),
        }
    }

    match (wan, singbox, tunnel, dns) {
        (Some(wan), Some(singbox), Some(tunnel), Some(dns)) => Ok(Observation {
            telemetry: Telemetry {
                wan,
                singbox,
                tunnel,
                dns,
                nodes,
                aux,
            },
            injected,
        }),
        _ => Err(ProbeError::Malformed),
    }
}

/// Parse a ground-truth injection label — the same wire spelling the record format
/// uses for a fault class. An unknown value is hostile input and is rejected.
fn parse_failure_class(value: &str) -> Result<FailureClass, ProbeError> {
    match value {
        "wan_down" => Ok(FailureClass::WanDown),
        "singbox_dead" => Ok(FailureClass::SingboxDead),
        "node_dead" => Ok(FailureClass::NodeDead),
        "node_degraded" => Ok(FailureClass::NodeDegraded),
        "dns_fail" => Ok(FailureClass::DnsFail),
        _ => Err(ProbeError::Malformed),
    }
}

fn parse_wan(value: &str) -> Result<Wan, ProbeError> {
    match value {
        "down" => Ok(Wan::Down),
        "unknown" => Ok(Wan::Unknown),
        other => match other.split_once(':') {
            Some(("up", secs)) => {
                let uptime_s = secs.parse::<u32>().map_err(|_| ProbeError::Malformed)?;
                Ok(Wan::Up { uptime_s })
            }
            _ => Err(ProbeError::Malformed),
        },
    }
}

fn parse_proc(value: &str) -> Result<Proc, ProbeError> {
    match value {
        "running" => Ok(Proc::Running),
        "dead" => Ok(Proc::Dead),
        "unknown" => Ok(Proc::Unknown),
        _ => Err(ProbeError::Malformed),
    }
}

fn parse_tunnel(value: &str) -> Result<Tunnel, ProbeError> {
    match value {
        "dead" => Ok(Tunnel::Dead),
        "unknown" => Ok(Tunnel::Unknown),
        other => match other.split_once(':') {
            Some(("ok", ms)) => {
                let latency = ms.parse::<u32>().map_err(|_| ProbeError::Malformed)?;
                Ok(Tunnel::Healthy {
                    latency: LatencyMs(latency),
                })
            }
            _ => Err(ProbeError::Malformed),
        },
    }
}

fn parse_dns(value: &str) -> Result<Dns, ProbeError> {
    match value {
        "ok" => Ok(Dns::Ok),
        "failing" => Ok(Dns::Failing),
        "unknown" => Ok(Dns::Unknown),
        _ => Err(ProbeError::Malformed),
    }
}

fn parse_node(value: &str) -> Result<NodeHealth, ProbeError> {
    let (id_str, lat_str) = value.split_once(':').ok_or(ProbeError::Malformed)?;
    let id = id_str.parse::<u8>().map_err(|_| ProbeError::Malformed)?;
    let latency = match lat_str {
        "x" => None,
        ms => Some(LatencyMs(
            ms.parse::<u32>().map_err(|_| ProbeError::Malformed)?,
        )),
    };
    Ok(NodeHealth {
        id: NodeId(id),
        latency,
    })
}

/// Live telemetry source: runs the contract on the router over SSH and parses it.
#[derive(Debug, Clone)]
pub struct SshTelemetry<'r, R: CommandRunner> {
    ssh: SshExec<'r, R>,
}

impl<'r, R: CommandRunner> SshTelemetry<'r, R> {
    #[must_use]
    pub fn new(ssh: SshExec<'r, R>) -> Self {
        Self { ssh }
    }
}

impl<R: CommandRunner> TelemetrySource for SshTelemetry<'_, R> {
    fn sample(&self) -> Result<Observation, ProbeError> {
        match self.ssh.exec(REMOTE_TELEMETRY_SCRIPT) {
            Ok(output) => parse_snapshot(&output.stdout_text()),
            // A transport failure is not a diagnosis: report it as an inability
            // to observe, which the supervisor treats as "all Unknown".
            Err(RunError::Timeout) => Err(ProbeError::Timeout),
            Err(_) => Err(ProbeError::Unreachable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_snapshot;
    use sovereign_core::domain::{Aux, Dns, LatencyMs, Proc, Tunnel, Wan, WanReach};
    use sovereign_core::error::ProbeError;

    const GOOD: &str = "wan=up:12345\nsingbox=running\ntunnel=ok:439\ndns=ok\n";

    #[test]
    fn parses_a_complete_healthy_snapshot() -> Result<(), ProbeError> {
        let t = parse_snapshot(GOOD)?.telemetry;
        assert_eq!(t.wan, Wan::Up { uptime_s: 12345 });
        assert_eq!(t.singbox, Proc::Running);
        assert_eq!(
            t.tunnel,
            Tunnel::Healthy {
                latency: LatencyMs(439)
            }
        );
        assert_eq!(t.dns, Dns::Ok);
        // No aux tokens in the snapshot -> everything reads "not measured".
        assert_eq!(t.aux, Aux::UNKNOWN);
        Ok(())
    }

    #[test]
    fn parses_auxiliary_signals() -> Result<(), ProbeError> {
        let text = format!("{GOOD}wanok=1\nload=42\nmem=151204\nactive=4\n");
        let t = parse_snapshot(&text)?.telemetry;
        assert_eq!(t.aux.wan_reach, WanReach::Ok);
        assert_eq!(t.aux.load_centi, Some(42));
        assert_eq!(t.aux.mem_free_kb, Some(151_204));
        assert_eq!(t.aux.active, Some(4));
        Ok(())
    }

    #[test]
    fn parses_and_validates_the_injection_label() -> Result<(), ProbeError> {
        use sovereign_core::failure::FailureClass;
        // A valid marker labels the observation as a deliberate injection.
        let obs = parse_snapshot(&format!("{GOOD}inject=singbox_dead\n"))?;
        assert_eq!(obs.injected, Some(FailureClass::SingboxDead));
        // No marker -> organic.
        assert_eq!(parse_snapshot(GOOD)?.injected, None);
        // A bogus class is hostile input and is rejected.
        assert_eq!(
            parse_snapshot(&format!("{GOOD}inject=melted\n")),
            Err(ProbeError::Malformed)
        );
        Ok(())
    }

    #[test]
    fn rejects_garbled_auxiliary_values() {
        for bad in ["wanok=perhaps", "load=heavy", "mem=lots", "active=abc"] {
            let text = format!("{GOOD}{bad}\n");
            assert_eq!(parse_snapshot(&text), Err(ProbeError::Malformed), "{bad}");
        }
    }

    #[test]
    fn parses_down_and_dead_states() -> Result<(), ProbeError> {
        let t = parse_snapshot("wan=down\nsingbox=dead\ntunnel=dead\ndns=failing\n")?.telemetry;
        assert_eq!(t.wan, Wan::Down);
        assert_eq!(t.singbox, Proc::Dead);
        assert_eq!(t.tunnel, Tunnel::Dead);
        assert_eq!(t.dns, Dns::Failing);
        Ok(())
    }

    #[test]
    fn rejects_a_missing_mandatory_field() {
        let text = "wan=up:1\nsingbox=running\ndns=ok\n"; // no tunnel
        assert_eq!(parse_snapshot(text), Err(ProbeError::Malformed));
    }

    #[test]
    fn rejects_a_garbage_value() {
        let text = "wan=sideways\nsingbox=running\ntunnel=ok:1\ndns=ok\n";
        assert_eq!(parse_snapshot(text), Err(ProbeError::Malformed));
    }

    #[test]
    fn rejects_a_non_numeric_latency() {
        let text = "wan=up:1\nsingbox=running\ntunnel=ok:fast\ndns=ok\n";
        assert_eq!(parse_snapshot(text), Err(ProbeError::Malformed));
    }

    #[test]
    fn rejects_an_unknown_key() {
        let text = format!("{GOOD}mystery=1\n");
        assert_eq!(parse_snapshot(&text), Err(ProbeError::Malformed));
    }

    #[test]
    fn parses_optional_node_lines() -> Result<(), ProbeError> {
        let text = format!("{GOOD}node=1:439\nnode=2:x\n");
        let t = parse_snapshot(&text)?.telemetry;
        let first = t.nodes.iter().flatten().count();
        assert_eq!(first, 2);
        Ok(())
    }

    #[test]
    fn tolerates_more_nodes_than_capacity() -> Result<(), ProbeError> {
        let mut text = String::from(GOOD);
        for i in 0..20u32 {
            // i stays below 20, so it fits a u8 node id; no arithmetic needed.
            text.push_str(&format!("node={i}:{i}\n"));
        }
        let t = parse_snapshot(&text)?.telemetry; // must not panic or overflow
        assert!(t.nodes.iter().flatten().count() <= t.nodes.len());
        Ok(())
    }
}
