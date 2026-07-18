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

/// Whether a remedy was actually carried out on the device, or only simulated.
///
/// This distinction is load-bearing for the dataset's honesty (the principle: make the
/// invalid state — "recorded as verified but never issued" — unrepresentable): an
/// observe-only run decides real actions but must not claim to have applied them,
/// or the trail teaches a future model a false action→outcome causality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actuation {
    /// The command was issued to the device and its post-condition verified.
    Applied,
    /// Observe-only mode: the remedy was decided but deliberately not issued.
    Simulated,
}

/// The ability to apply a remediation and confirm it took effect.
///
/// Actuation must be atomic and self-verifying (the principle: ACID). An
/// `Ok(Actuation::Applied)` means the post-condition was checked — not merely that
/// a command was sent; an `Ok(Actuation::Simulated)` means observe-only, nothing
/// was issued. On any doubt the implementation returns [`ActError::Unverified`] so
/// the caller treats the change as not applied.
pub trait Actuator {
    /// Select a different exit node, avoiding the one that just failed.
    fn switch_node(&self, avoid: NodeId) -> Result<Actuation, ActError>;
    /// Restart the tunnel process and verify it comes back up.
    fn restart_singbox(&self) -> Result<Actuation, ActError>;
    /// Bounce the WAN interface and verify the link re-establishes.
    fn bounce_wan(&self) -> Result<Actuation, ActError>;
    /// Cut over to the backup uplink and verify reachability through it.
    fn failover_backup(&self) -> Result<Actuation, ActError>;
    /// Reboot the device. Terminal remedy; verification is post-boot.
    fn reboot(&self) -> Result<Actuation, ActError>;
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
