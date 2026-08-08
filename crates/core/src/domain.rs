// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vocabulary of the sovereign core.
//!
//! Types here make invalid states unrepresentable: a subsystem is
//! exactly one of its enum variants, never a contradictory combination of
//! booleans. There is no way to construct, say, a WAN that is simultaneously up
//! and down — the compiler forbids it.

/// Maximum number of tunnel exit nodes the core reasons about in one cycle.
///
/// A fixed capacity keeps the core allocation-free (zero hot-path
/// allocation) and gives every loop over nodes a statically provable bound
/// (bounded execution).
pub const MAX_NODES: usize = 8;

/// Monotonic logical time: one tick is one supervision cycle. Unified with the
/// toolkit's clock (`sovds::record::Tick`) so a record's `tick` and the FSM's
/// `now`/`until` are the same type — one logical clock, no conversions.
pub use sovds::record::Tick;

/// Round-trip latency of a probe, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LatencyMs(pub u32);

/// Opaque identity of a tunnel node.
///
/// The core reasons about nodes by id only; mapping an id to a provider-specific
/// server name or URL is strictly an adapter concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId(pub u8);

/// State of the upstream WAN link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wan {
    /// Link is up and has carried a session for `uptime_s` seconds.
    Up { uptime_s: u32 },
    /// Link is down (no session / no reachability).
    Down,
    /// Could not be determined this cycle; treated as "not a failure" so a probe
    /// gap never triggers a disruptive action on its own.
    Unknown,
}

/// Liveness of the tunnel process (sing-box).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proc {
    Running,
    Dead,
    Unknown,
}

/// End-to-end health of the tunnel data path, as measured through the currently
/// selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tunnel {
    /// Path works within the healthy latency budget.
    Healthy {
        latency: LatencyMs,
    },
    /// Path answers but is slower than the budget (a failing/overloaded node).
    Degraded {
        latency: LatencyMs,
    },
    /// Path does not carry traffic at all.
    Dead,
    Unknown,
}

/// Result of a DNS / FakeIP resolution probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dns {
    Ok,
    Failing,
    Unknown,
}

/// Health of a single exit node as measured this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeHealth {
    pub id: NodeId,
    /// `None` means "no successful measurement this cycle" — absence of data,
    /// deliberately distinct from a measured-but-slow value.
    pub latency: Option<LatencyMs>,
}

/// Whether traffic actually flows THROUGH the WAN — measured by fetching a
/// neutral external host, independent of the PPPoE session state. This is the
/// signal that catches the "session up but blackholed" failure a session-status
/// check cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WanReach {
    /// A neutral external host answered through the WAN.
    Ok,
    /// The WAN session may be up, but nothing external answers.
    Blackhole,
    /// Not measured this cycle.
    Unknown,
}

/// Auxiliary health signals grouped in one sub-struct so the observation can
/// grow without touching every `Telemetry` constructor again (new signals land
/// here; call sites keep saying `aux: Aux::UNKNOWN`).
///
/// `Option`/`Unknown` uniformly means "not measured" — old records and failed
/// sub-probes parse to exactly this value, never to a fake measurement.
///
/// Packet loss / jitter are deliberately absent: honest loss needs multiple
/// probes per cycle (a real load multiplier), and jitter is derivable offline
/// from the existing 1/min latency series — adding either now would be
/// unjustified complexity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aux {
    /// Real reachability through the WAN (see [`WanReach`]).
    pub wan_reach: WanReach,
    /// Router 1-minute load average, times 100 (integer fixed-point).
    pub load_centi: Option<u32>,
    /// Router `MemAvailable`, in KiB.
    pub mem_free_kb: Option<u32>,
    /// The exit node the urltest group currently routes through.
    pub active: Option<u8>,
}

impl Aux {
    /// The "nothing measured" value — what old records parse to, and what every
    /// constructor uses until a probe fills the fields in.
    pub const UNKNOWN: Self = Self {
        wan_reach: WanReach::Unknown,
        load_centi: None,
        mem_free_kb: None,
        active: None,
    };
}

/// One immutable observation of the whole system.
///
/// This value is the sole input to the decision core (state is passed
/// explicitly). It is `Copy` and heap-free: the node table is a fixed array
/// whose unused slots are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Telemetry {
    pub wan: Wan,
    pub singbox: Proc,
    pub tunnel: Tunnel,
    pub dns: Dns,
    pub nodes: [Option<NodeHealth>; MAX_NODES],
    /// Auxiliary signals (WAN reachability, router resources, active node).
    pub aux: Aux,
}
