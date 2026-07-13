//! Ports: the contracts the core requires of the outside world.
//!
//! Adapters (SSH, ntfy, files) implement these traits; the core depends only on
//! the traits, never on their implementations (the principle: agnostic core, via
//! dependency inversion). Because the outside world is presumed hostile
//! (the principle), every method returns a `Result` — there is no infallible I/O.
//!
//! These are contracts only. No adapter lives in this crate: keeping I/O out of
//! the core is what lets the same decision logic run on a laptop today and on a
//! home server tomorrow without changing a line.

use crate::domain::{NodeId, Telemetry};
use crate::error::{ActError, ProbeError};
use crate::fsm::Reason;

/// A source of system observations.
///
/// Implementations must enforce a hard timeout internally so a hung transport can
/// never stall the supervision loop (the principle: bounded execution). A returned
/// [`Telemetry`] must already have been validated at the boundary — the core
/// trusts it completely.
pub trait TelemetrySource {
    /// Take one complete, validated observation of the system.
    fn sample(&self) -> Result<Telemetry, ProbeError>;
}

/// The ability to apply a remediation and confirm it took effect.
///
/// Actuation must be atomic and self-verifying (the principle: ACID). An `Ok` means
/// the post-condition was checked — not merely that a command was sent. On any
/// doubt the implementation returns [`ActError::Unverified`] so the caller treats
/// the change as not applied.
pub trait Actuator {
    /// Select a different exit node, avoiding the one that just failed.
    fn switch_node(&self, avoid: NodeId) -> Result<(), ActError>;
    /// Restart the tunnel process and verify it comes back up.
    fn restart_singbox(&self) -> Result<(), ActError>;
    /// Bounce the WAN interface and verify the link re-establishes.
    fn bounce_wan(&self) -> Result<(), ActError>;
    /// Cut over to the backup uplink and verify reachability through it.
    fn failover_backup(&self) -> Result<(), ActError>;
    /// Reboot the device. Terminal remedy; verification is post-boot.
    fn reboot(&self) -> Result<(), ActError>;
}

/// A channel for human-legible notifications.
///
/// Notification is a courtesy, not a dependency: a failure to notify must never
/// escalate into a core failure (the principle: graceful degradation). Callers log
/// the [`ActError`] and carry on.
pub trait Notifier {
    /// Emit a human-facing note describing why the supervisor acted.
    fn notify(&self, event: Reason) -> Result<(), ActError>;
}
