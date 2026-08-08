// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
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
use crate::failure::FailureClass;
use crate::fsm::Reason;

/// One observation of the system plus its ground-truth injection label.
///
/// `telemetry` is the health signal the core reasons over. `injected` is
/// out-of-band ground truth — the fault class we deliberately induced this cycle
/// (read from the router's injection marker), or `None` in organic operation. The
/// core ignores `injected` for decisions; it rides through to the record so the
/// dataset can tell "we broke it on purpose" from a real fault (the principle: the two
/// are different states and must be representable as such).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub telemetry: Telemetry,
    pub injected: Option<FailureClass>,
}

impl Observation {
    /// An organic observation (nothing injected).
    #[must_use]
    pub const fn organic(telemetry: Telemetry) -> Self {
        Self {
            telemetry,
            injected: None,
        }
    }
}

/// A source of system observations.
///
/// Implementations must enforce a hard timeout internally so a hung transport can
/// never stall the supervision loop (the principle: bounded execution). A returned
/// [`Telemetry`] must already have been validated at the boundary — the core
/// trusts it completely.
pub trait TelemetrySource {
    /// Take one complete, validated observation of the system, including any
    /// ground-truth injection label read from the router this cycle.
    fn sample(&self) -> Result<Observation, ProbeError>;
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
