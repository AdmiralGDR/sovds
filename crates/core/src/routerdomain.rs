// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `RouterDomain` — the router as a [`sovds::Domain`].
//!
//! This is the seam that makes the Sovereign Router Brain a *reference
//! implementation* of the generic dataset toolkit rather than a bespoke system.
//! It maps the router's world onto the three toolkit types:
//!
//! * `Obs` = [`Telemetry`] — one raw observation of the router.
//! * `Fault` = [`FailureClass`] — the fault vocabulary.
//! * `Annot` = [`RouterAnnot`] — the supervisor's per-cycle decision (adaptive
//!   verdict, FSM state, reason, action, outcome), carried alongside the raw
//!   telemetry so the dataset records what the brain *did*, not just what it saw.
//!
//! All the wire-format token logic lives here, so the format is defined in exactly
//! one place. The toolkit owns the framing tokens (`t ts probed class src inject`);
//! this owns the router's observation and annotation tokens.

use crate::domain::{
    Aux, Dns, LatencyMs, NodeHealth, NodeId, Proc, Telemetry, Tick, Tunnel, Wan, WanReach,
    MAX_NODES,
};
use crate::failure::FailureClass;
use crate::fleet::NodeVerdict;
use crate::fsm::{Action, Reason, State};
use core::fmt::Write;
use sovds::domain::{Accept, Domain, ParseError};

/// What became of the action taken this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    /// No action was taken (the decision was to observe).
    #[default]
    NotActed,
    /// An action was applied and its post-condition verified.
    Verified,
    /// An action was attempted but could not be verified.
    Unverified,
    /// An action was decided but deliberately not issued (observe-only / dry-run).
    Simulated,
}

/// The supervisor's per-cycle decision, carried alongside the raw telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterAnnot {
    /// The adaptive verdict on the active path this cycle.
    pub verdict: NodeVerdict,
    /// The state the machine moved into.
    pub state: State,
    pub reason: Reason,
    pub action: Action,
    pub outcome: Outcome,
}

impl Default for RouterAnnot {
    fn default() -> Self {
        Self {
            verdict: NodeVerdict::Warming,
            state: State::Healthy,
            reason: Reason::Nominal,
            action: Action::Observe,
            outcome: Outcome::NotActed,
        }
    }
}

/// The router domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterDomain;

/// Parse accumulator: folds the router's obs+annot tokens as they are seen.
pub struct RouterBuilder {
    wan: Option<Wan>,
    singbox: Option<Proc>,
    tunnel: Option<Tunnel>,
    dns: Option<Dns>,
    nodes: [Option<NodeHealth>; MAX_NODES],
    aux: Aux,
    verdict: Option<NodeVerdict>,
    state: Option<State>,
    reason: Option<Reason>,
    action: Option<Action>,
    outcome: Option<Outcome>,
}

impl Default for RouterBuilder {
    fn default() -> Self {
        Self {
            wan: None,
            singbox: None,
            tunnel: None,
            dns: None,
            nodes: [None; MAX_NODES],
            aux: Aux::UNKNOWN,
            verdict: None,
            state: None,
            reason: None,
            action: None,
            outcome: None,
        }
    }
}

impl Domain for RouterDomain {
    type Obs = Telemetry;
    type Fault = FailureClass;
    type Annot = RouterAnnot;
    type Builder = RouterBuilder;

    fn healthy() -> FailureClass {
        FailureClass::None
    }

    fn faults() -> &'static [FailureClass] {
        &[
            FailureClass::None,
            FailureClass::WanDown,
            FailureClass::SingboxDead,
            FailureClass::NodeDead,
            FailureClass::NodeDegraded,
            FailureClass::DnsFail,
        ]
    }

    fn fault_label(f: FailureClass) -> &'static str {
        failure_label(f)
    }

    fn parse_fault(s: &str) -> Option<FailureClass> {
        parse_failure(s)
    }

    fn write_obs<W: Write>(w: &mut W, t: &Telemetry) -> core::fmt::Result {
        w.write_str(" wan=")?;
        write_wan(w, t.wan)?;
        w.write_str(" sb=")?;
        w.write_str(proc_label(t.singbox))?;
        w.write_str(" tun=")?;
        write_tunnel(w, t.tunnel)?;
        w.write_str(" dns=")?;
        w.write_str(dns_label(t.dns))?;
        w.write_str(" nodes=")?;
        write_nodes(w, &t.nodes)?;
        write_aux(w, &t.aux)
    }

    fn write_annot<W: Write>(w: &mut W, a: &RouterAnnot) -> core::fmt::Result {
        w.write_str(" verdict=")?;
        w.write_str(verdict_label(a.verdict))?;
        w.write_str(" state=")?;
        write_state(w, a.state)?;
        w.write_str(" reason=")?;
        write_reason(w, a.reason)?;
        w.write_str(" action=")?;
        w.write_str(action_label(a.action))?;
        w.write_str(" outcome=")?;
        w.write_str(outcome_label(a.outcome))
    }

    fn accept(b: &mut RouterBuilder, key: &str, value: &str) -> Result<Accept, ParseError> {
        match key {
            "wan" => b.wan = Some(parse_wan(value)?),
            "sb" => b.singbox = Some(parse_proc(value).ok_or(ParseError::BadField)?),
            "tun" => b.tunnel = Some(parse_tunnel(value)?),
            "dns" => b.dns = Some(parse_dns(value).ok_or(ParseError::BadField)?),
            "nodes" => b.nodes = parse_nodes(value)?,
            "wanok" => b.aux.wan_reach = parse_wanok(value)?,
            "load" => b.aux.load_centi = parse_opt_u32(value)?,
            "mem" => b.aux.mem_free_kb = parse_opt_u32(value)?,
            "active" => b.aux.active = parse_opt_u8(value)?,
            "verdict" => b.verdict = Some(parse_verdict(value).ok_or(ParseError::BadField)?),
            "state" => b.state = Some(parse_state(value)?),
            "reason" => b.reason = Some(parse_reason(value)?),
            "action" => b.action = Some(parse_action(value).ok_or(ParseError::BadField)?),
            "outcome" => b.outcome = Some(parse_outcome(value).ok_or(ParseError::BadField)?),
            _ => return Ok(Accept::Unknown),
        }
        Ok(Accept::Consumed)
    }

    fn finish(b: RouterBuilder) -> Result<(Telemetry, RouterAnnot), ParseError> {
        let telemetry = Telemetry {
            wan: b.wan.ok_or(ParseError::MissingField)?,
            singbox: b.singbox.ok_or(ParseError::MissingField)?,
            tunnel: b.tunnel.ok_or(ParseError::MissingField)?,
            dns: b.dns.ok_or(ParseError::MissingField)?,
            nodes: b.nodes,
            aux: b.aux,
        };
        let annot = RouterAnnot {
            verdict: b.verdict.ok_or(ParseError::MissingField)?,
            state: b.state.ok_or(ParseError::MissingField)?,
            reason: b.reason.ok_or(ParseError::MissingField)?,
            action: b.action.ok_or(ParseError::MissingField)?,
            outcome: b.outcome.ok_or(ParseError::MissingField)?,
        };
        Ok((telemetry, annot))
    }

    fn signal_names() -> &'static [&'static str] {
        &[
            "wan_up",
            "singbox_up",
            "tunnel_up",
            "tunnel_ms",
            "dns_ok",
            "nodes_seen",
            "wanok",
            "load",
            "mem",
            "active",
        ]
    }

    /// Raw scalar telemetry, one value per name above. `-1` means "not measured"
    /// (a `-`/`Unknown`/dead signal), so a column is always present and honest.
    fn signals(t: &Telemetry, sink: &mut dyn FnMut(i64)) {
        sink(match t.wan {
            Wan::Up { .. } => 1,
            Wan::Down => 0,
            Wan::Unknown => -1,
        });
        sink(match t.singbox {
            Proc::Running => 1,
            Proc::Dead => 0,
            Proc::Unknown => -1,
        });
        sink(match t.tunnel {
            Tunnel::Healthy { .. } | Tunnel::Degraded { .. } => 1,
            Tunnel::Dead => 0,
            Tunnel::Unknown => -1,
        });
        sink(match t.tunnel {
            Tunnel::Healthy { latency } | Tunnel::Degraded { latency } => i64::from(latency.0),
            Tunnel::Dead | Tunnel::Unknown => -1,
        });
        sink(match t.dns {
            Dns::Ok => 1,
            Dns::Failing => 0,
            Dns::Unknown => -1,
        });
        let seen = t
            .nodes
            .iter()
            .flatten()
            .filter(|n| n.latency.is_some())
            .count();
        sink(u8::try_from(seen).map(i64::from).unwrap_or(-1));
        sink(match t.aux.wan_reach {
            WanReach::Ok => 1,
            WanReach::Blackhole => 0,
            WanReach::Unknown => -1,
        });
        sink(t.aux.load_centi.map(i64::from).unwrap_or(-1));
        sink(t.aux.mem_free_kb.map(i64::from).unwrap_or(-1));
        sink(t.aux.active.map(i64::from).unwrap_or(-1));
    }
}

// --- serialization (moved verbatim from the old record module) ---------------

fn write_wan<W: Write>(w: &mut W, wan: Wan) -> core::fmt::Result {
    match wan {
        Wan::Up { uptime_s } => write!(w, "up:{uptime_s}"),
        Wan::Down => w.write_str("down"),
        Wan::Unknown => w.write_str("unknown"),
    }
}

fn write_nodes<W: Write>(w: &mut W, nodes: &[Option<NodeHealth>; MAX_NODES]) -> core::fmt::Result {
    let mut any = false;
    for node in nodes.iter().flatten() {
        if any {
            w.write_str(",")?;
        }
        any = true;
        match node.latency {
            Some(latency) => write!(w, "{}:{}", node.id.0, latency.0)?,
            None => write!(w, "{}:x", node.id.0)?,
        }
    }
    if !any {
        w.write_str("-")?;
    }
    Ok(())
}

fn write_tunnel<W: Write>(w: &mut W, tunnel: Tunnel) -> core::fmt::Result {
    match tunnel {
        Tunnel::Healthy { latency } => write!(w, "ok:{}", latency.0),
        Tunnel::Degraded { latency } => write!(w, "deg:{}", latency.0),
        Tunnel::Dead => w.write_str("dead"),
        Tunnel::Unknown => w.write_str("unknown"),
    }
}

fn write_aux<W: Write>(w: &mut W, aux: &Aux) -> core::fmt::Result {
    match aux.wan_reach {
        WanReach::Ok => w.write_str(" wanok=1")?,
        WanReach::Blackhole => w.write_str(" wanok=0")?,
        WanReach::Unknown => w.write_str(" wanok=-")?,
    }
    match aux.load_centi {
        Some(v) => write!(w, " load={v}")?,
        None => w.write_str(" load=-")?,
    }
    match aux.mem_free_kb {
        Some(v) => write!(w, " mem={v}")?,
        None => w.write_str(" mem=-")?,
    }
    match aux.active {
        Some(v) => write!(w, " active={v}"),
        None => w.write_str(" active=-"),
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
        } => write!(
            w,
            "cooldown:{}:{}:{}",
            failure_label(failing),
            level,
            until.0
        ),
    }
}

fn write_reason<W: Write>(w: &mut W, reason: Reason) -> core::fmt::Result {
    match reason {
        Reason::Nominal => w.write_str("nominal"),
        Reason::FirstSuspicion(class) => write!(w, "first_suspicion:{}", failure_label(class)),
        Reason::AwaitingConfirmation { failing, seen } => {
            write!(w, "awaiting:{}:{}", failure_label(failing), seen)
        }
        Reason::Recovered => w.write_str("recovered"),
        Reason::Confirmed { failing, level } => {
            write!(w, "confirmed:{}:{}", failure_label(failing), level)
        }
        Reason::CoolingDown => w.write_str("cooling"),
        Reason::Escalated { failing, level } => {
            write!(w, "escalated:{}:{}", failure_label(failing), level)
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
        Outcome::Simulated => "dryrun",
    }
}

// --- parsing (moved verbatim from the old record module) ---------------------

fn parse_u64(value: &str) -> Result<u64, ParseError> {
    value.parse::<u64>().map_err(|_| ParseError::BadField)
}

fn parse_u8(value: &str) -> Result<u8, ParseError> {
    value.parse::<u8>().map_err(|_| ParseError::BadField)
}

fn parse_u32(value: &str) -> Result<u32, ParseError> {
    value.parse::<u32>().map_err(|_| ParseError::BadField)
}

fn parse_wan(value: &str) -> Result<Wan, ParseError> {
    match value {
        "down" => Ok(Wan::Down),
        "unknown" => Ok(Wan::Unknown),
        other => {
            let (tag, rest) = other.split_once(':').ok_or(ParseError::BadField)?;
            if tag != "up" {
                return Err(ParseError::BadField);
            }
            Ok(Wan::Up {
                uptime_s: parse_u32(rest)?,
            })
        }
    }
}

fn parse_tunnel(value: &str) -> Result<Tunnel, ParseError> {
    match value {
        "dead" => Ok(Tunnel::Dead),
        "unknown" => Ok(Tunnel::Unknown),
        other => {
            let (tag, rest) = other.split_once(':').ok_or(ParseError::BadField)?;
            let latency = LatencyMs(parse_u32(rest)?);
            match tag {
                "ok" => Ok(Tunnel::Healthy { latency }),
                "deg" => Ok(Tunnel::Degraded { latency }),
                _ => Err(ParseError::BadField),
            }
        }
    }
}

fn parse_state(value: &str) -> Result<State, ParseError> {
    let mut parts = value.split(':');
    match parts.next() {
        Some("healthy") => Ok(State::Healthy),
        Some("suspect") => {
            let failing = next_failure(&mut parts)?;
            let seen = next_u8(&mut parts)?;
            Ok(State::Suspect { failing, seen })
        }
        Some("cooldown") => {
            let failing = next_failure(&mut parts)?;
            let level = next_u8(&mut parts)?;
            let until = Tick(next_u64(&mut parts)?);
            Ok(State::Cooldown {
                failing,
                level,
                until,
            })
        }
        _ => Err(ParseError::BadField),
    }
}

fn parse_reason(value: &str) -> Result<Reason, ParseError> {
    let mut parts = value.split(':');
    match parts.next() {
        Some("nominal") => Ok(Reason::Nominal),
        Some("recovered") => Ok(Reason::Recovered),
        Some("cooling") => Ok(Reason::CoolingDown),
        Some("first_suspicion") => Ok(Reason::FirstSuspicion(next_failure(&mut parts)?)),
        Some("awaiting") => {
            let failing = next_failure(&mut parts)?;
            let seen = next_u8(&mut parts)?;
            Ok(Reason::AwaitingConfirmation { failing, seen })
        }
        Some("confirmed") => {
            let failing = next_failure(&mut parts)?;
            let level = next_u8(&mut parts)?;
            Ok(Reason::Confirmed { failing, level })
        }
        Some("escalated") => {
            let failing = next_failure(&mut parts)?;
            let level = next_u8(&mut parts)?;
            Ok(Reason::Escalated { failing, level })
        }
        _ => Err(ParseError::BadField),
    }
}

fn next_failure<'a>(parts: &mut impl Iterator<Item = &'a str>) -> Result<FailureClass, ParseError> {
    parts
        .next()
        .and_then(parse_failure)
        .ok_or(ParseError::BadField)
}

fn next_u8<'a>(parts: &mut impl Iterator<Item = &'a str>) -> Result<u8, ParseError> {
    match parts.next() {
        Some(s) => parse_u8(s),
        None => Err(ParseError::BadField),
    }
}

fn next_u64<'a>(parts: &mut impl Iterator<Item = &'a str>) -> Result<u64, ParseError> {
    match parts.next() {
        Some(s) => parse_u64(s),
        None => Err(ParseError::BadField),
    }
}

fn parse_proc(value: &str) -> Option<Proc> {
    match value {
        "running" => Some(Proc::Running),
        "dead" => Some(Proc::Dead),
        "unknown" => Some(Proc::Unknown),
        _ => None,
    }
}

fn parse_dns(value: &str) -> Option<Dns> {
    match value {
        "ok" => Some(Dns::Ok),
        "failing" => Some(Dns::Failing),
        "unknown" => Some(Dns::Unknown),
        _ => None,
    }
}

fn parse_verdict(value: &str) -> Option<NodeVerdict> {
    match value {
        "warming" => Some(NodeVerdict::Warming),
        "healthy" => Some(NodeVerdict::Healthy),
        "slow" => Some(NodeVerdict::Slow),
        "throttled" => Some(NodeVerdict::Throttled),
        "down" => Some(NodeVerdict::Down),
        _ => None,
    }
}

fn parse_failure(value: &str) -> Option<FailureClass> {
    match value {
        "none" => Some(FailureClass::None),
        "wan_down" => Some(FailureClass::WanDown),
        "singbox_dead" => Some(FailureClass::SingboxDead),
        "node_dead" => Some(FailureClass::NodeDead),
        "node_degraded" => Some(FailureClass::NodeDegraded),
        "dns_fail" => Some(FailureClass::DnsFail),
        _ => None,
    }
}

fn parse_action(value: &str) -> Option<Action> {
    match value {
        "observe" => Some(Action::Observe),
        "switch_node" => Some(Action::SwitchNode),
        "restart_singbox" => Some(Action::RestartSingbox),
        "bounce_wan" => Some(Action::BounceWan),
        "failover_backup" => Some(Action::FailoverBackup),
        "reboot" => Some(Action::Reboot),
        _ => None,
    }
}

fn parse_outcome(value: &str) -> Option<Outcome> {
    match value {
        "-" => Some(Outcome::NotActed),
        "verified" => Some(Outcome::Verified),
        "unverified" => Some(Outcome::Unverified),
        "dryrun" => Some(Outcome::Simulated),
        _ => None,
    }
}

fn parse_wanok(value: &str) -> Result<WanReach, ParseError> {
    match value {
        "1" => Ok(WanReach::Ok),
        "0" => Ok(WanReach::Blackhole),
        "-" => Ok(WanReach::Unknown),
        _ => Err(ParseError::BadField),
    }
}

fn parse_opt_u32(value: &str) -> Result<Option<u32>, ParseError> {
    match value {
        "-" => Ok(None),
        other => other
            .parse::<u32>()
            .map(Some)
            .map_err(|_| ParseError::BadField),
    }
}

fn parse_opt_u8(value: &str) -> Result<Option<u8>, ParseError> {
    match value {
        "-" => Ok(None),
        other => other
            .parse::<u8>()
            .map(Some)
            .map_err(|_| ParseError::BadField),
    }
}

fn parse_nodes(value: &str) -> Result<[Option<NodeHealth>; MAX_NODES], ParseError> {
    let mut nodes: [Option<NodeHealth>; MAX_NODES] = [None; MAX_NODES];
    if value == "-" {
        return Ok(nodes);
    }
    let mut slot = 0usize;
    for entry in value.split(',') {
        let (id_str, lat_str) = entry.split_once(':').ok_or(ParseError::BadField)?;
        let id = NodeId(parse_u8(id_str)?);
        let latency = match lat_str {
            "x" => None,
            other => Some(LatencyMs(parse_u32(other)?)),
        };
        if let Some(cell) = nodes.get_mut(slot) {
            *cell = Some(NodeHealth { id, latency });
            slot = slot.saturating_add(1);
        }
    }
    Ok(nodes)
}
