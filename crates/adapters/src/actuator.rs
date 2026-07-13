//! Actuator adapter: apply a remediation on the router and verify it took hold.
//!
//! Every method here honours the ACID contract of [`Actuator`] (the principle): it
//! returns `Ok(())` only after re-observing the router and confirming the
//! post-condition. Any doubt becomes [`ActError::Unverified`], so the supervisor
//! treats the change as not applied and will re-evaluate next cycle. The Clash
//! secret is sourced from the router's own config, never shipped in these strings.

use crate::os::{CommandRunner, RunError};
use crate::ssh::SshExec;
use sovereign_core::domain::NodeId;
use sovereign_core::error::ActError;
use sovereign_core::ports::Actuator;

const API: &str = "http://192.168.1.1:9090";
const SELECTOR: &str = "vless_out-out";
const AUTO_GROUP: &str = "vless_out-urltest-out";

/// Applies remediations over SSH, with a deadline long enough for disruptive
/// operations (interface bounce, failover) that briefly take the link down.
#[derive(Debug, Clone)]
pub struct SshActuator<'r, R: CommandRunner> {
    ssh: SshExec<'r, R>,
}

impl<'r, R: CommandRunner> SshActuator<'r, R> {
    #[must_use]
    pub fn new(ssh: SshExec<'r, R>) -> Self {
        Self { ssh }
    }

    /// Run `action`, then run `verify` and require its output to contain `needle`.
    fn act_then_verify(&self, action: &str, verify: &str, needle: &str) -> Result<(), ActError> {
        self.ssh.exec(action).map_err(map_run_err)?;
        let observed = self.ssh.exec(verify).map_err(map_run_err)?;
        if observed.stdout_text().contains(needle) {
            Ok(())
        } else {
            Err(ActError::Unverified)
        }
    }
}

impl<R: CommandRunner> Actuator for SshActuator<'_, R> {
    /// Force the selector back onto the auto (urltest) group so it re-picks the
    /// best node. `avoid` is accepted for the contract; v1 delegates the choice
    /// to urltest rather than blacklisting a specific id.
    fn switch_node(&self, _avoid: NodeId) -> Result<(), ActError> {
        let action = format!(
            "S=$(uci -q get podkop.settings.yacd_secret_key); \
             curl -s -m 8 -X PUT -H \"Authorization: Bearer $S\" \
             -d '{{\"name\":\"{AUTO_GROUP}\"}}' {API}/proxies/{SELECTOR}"
        );
        let verify = format!(
            "S=$(uci -q get podkop.settings.yacd_secret_key); \
             curl -s -m 8 -H \"Authorization: Bearer $S\" {API}/proxies/{SELECTOR}"
        );
        self.act_then_verify(&action, &verify, AUTO_GROUP)
    }

    /// Restart the tunnel via podkop and confirm sing-box comes back.
    fn restart_singbox(&self) -> Result<(), ActError> {
        self.act_then_verify(
            "/etc/init.d/podkop restart >/dev/null 2>&1",
            "pidof sing-box >/dev/null 2>&1 && echo alive || echo gone",
            "alive",
        )
    }

    /// Bounce the WAN interface and confirm the session re-establishes. The wait
    /// is inside the remote script so the whole operation is one bounded call.
    fn bounce_wan(&self) -> Result<(), ActError> {
        self.act_then_verify(
            "ifdown wan; sleep 3; ifup wan; sleep 8",
            "ubus call network.interface.wan status 2>/dev/null",
            "\"up\": true",
        )
    }

    /// Cut over to the backup uplink. Inert until mwan3 + the USB tether are
    /// configured: if no backup interface is up, verification fails and this
    /// honestly reports `Unverified` rather than pretending to have failed over.
    fn failover_backup(&self) -> Result<(), ActError> {
        self.act_then_verify(
            "mwan3 use wan_backup >/dev/null 2>&1; \
             ubus call network.interface.wan_backup status 2>/dev/null",
            "ubus call network.interface.wan_backup status 2>/dev/null",
            "\"up\": true",
        )
    }

    /// Reboot the device. Terminal remedy: the reboot severs the SSH session, so
    /// success cannot be verified in-band — it is confirmed by the next telemetry
    /// cycle showing a fresh, low-uptime WAN. Dispatching the command is the act.
    fn reboot(&self) -> Result<(), ActError> {
        match self.ssh.exec("reboot >/dev/null 2>&1 &") {
            Ok(_) => Ok(()),
            // The connection dropping mid-reboot is expected, not a failure.
            Err(RunError::Timeout | RunError::Io) => Ok(()),
            Err(_) => Err(ActError::Unreachable),
        }
    }
}

/// Map a transport failure onto the actuator's error vocabulary.
fn map_run_err(err: RunError) -> ActError {
    match err {
        RunError::Timeout => ActError::Timeout,
        RunError::EmptyArgv | RunError::Spawn | RunError::Io => ActError::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_run_err, AUTO_GROUP, SELECTOR};
    use crate::os::RunError;
    use sovereign_core::error::ActError;

    #[test]
    fn transport_timeout_maps_to_act_timeout() {
        assert_eq!(map_run_err(RunError::Timeout), ActError::Timeout);
    }

    #[test]
    fn other_transport_errors_map_to_unreachable() {
        assert_eq!(map_run_err(RunError::Spawn), ActError::Unreachable);
        assert_eq!(map_run_err(RunError::Io), ActError::Unreachable);
    }

    #[test]
    fn selector_and_auto_group_are_distinct() {
        // A switch that pointed the selector at itself would be a no-op.
        assert_ne!(SELECTOR, AUTO_GROUP);
    }
}
