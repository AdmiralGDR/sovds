//! The vocabulary of the sovereign core.
//!
//! Types here make invalid states unrepresentable (the principle): a subsystem is
//! exactly one of its enum variants, never a contradictory combination of
//! booleans. There is no way to construct, say, a WAN that is simultaneously up
//! and down — the compiler forbids it.

/// Maximum number of tunnel exit nodes the core reasons about in one cycle.
///
/// A fixed capacity keeps the core allocation-free (the principle: zero hot-path
/// allocation) and gives every loop over nodes a statically provable bound
/// (the principle: bounded execution).
pub const MAX_NODES: usize = 8;

/// Monotonic logical time: one tick is one supervision cycle.
///
/// The core never reads a real clock (the principle: agnostic core) — the caller
/// injects `now`, which makes every decision reproducible in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick(pub u64);

impl Tick {
    /// Advance by `n` ticks, saturating instead of overflowing so time is always
    /// well defined (the principle: no undefined arithmetic).
    #[must_use]
    pub const fn saturating_add(self, n: u64) -> Self {
        Self(self.0.saturating_add(n))
    }
}

/// Round-trip latency of a probe, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LatencyMs(pub u32);

/// Opaque identity of a tunnel node.
///
/// The core reasons about nodes by id only; mapping an id to a provider-specific
/// server name or URL is strictly an adapter concern (the principle).
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

/// One immutable observation of the whole system.
///
/// This value is the sole input to the decision core (the principle: state is passed
/// explicitly). It is `Copy` and heap-free: the node table is a fixed array
/// whose unused slots are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Telemetry {
    pub wan: Wan,
    pub singbox: Proc,
    pub tunnel: Tunnel,
    pub dns: Dns,
    pub nodes: [Option<NodeHealth>; MAX_NODES],
}
