// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! SSH bridge: build the exact `ssh` invocation for running a command on the
//! router, and execute it through a [`CommandRunner`].
//!
//! The OpenSSH client is quarantined behind [`SshConfig`] (the principle): the rest of
//! the system never sees a raw command line. Password auth is driven by an
//! askpass helper whose path is passed in the environment, never on the argv, so
//! the secret never appears in a process list.

use crate::os::{CommandOutput, CommandRunner, RunError};
use std::time::Duration;

/// Immutable connection parameters for reaching the router.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// Router address (e.g. `192.168.1.1` or a Tailscale name).
    pub host: String,
    /// Login user (currently `root`).
    pub user: String,
    /// Optional source address to bind — the workstation is multi-homed and the
    /// wrong default route would otherwise shadow the router (a real failure we
    /// hit before).
    pub bind: Option<String>,
    /// Path to an askpass helper that prints the password on stdout. Kept out of
    /// the argv so the password never leaks into `ps`.
    pub askpass: Option<String>,
    /// Hard connect timeout handed to ssh itself, in seconds.
    pub connect_timeout_s: u32,
}

/// Build the argv to run `remote` on the router. Pure and total: the precise
/// command line is unit-testable without opening a socket.
///
/// The command is wrapped in `setsid -w` so ssh has no controlling terminal and
/// will consult `SSH_ASKPASS` for the password (see [`ssh_env`]).
#[must_use]
pub fn build_remote_argv(cfg: &SshConfig, remote: &str) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    argv.push("setsid".to_string());
    argv.push("-w".to_string());
    argv.push("ssh".to_string());

    if let Some(bind) = &cfg.bind {
        argv.push("-b".to_string());
        argv.push(bind.clone());
    }

    // Non-interactive, LAN/Tailscale-trusted options. Host-key checking is off
    // because the router key rotates on reflash and the link itself is trusted —
    // a deliberate, documented trade-off, not an oversight.
    for option in [
        "StrictHostKeyChecking=no",
        "UserKnownHostsFile=/dev/null",
        "GlobalKnownHostsFile=/dev/null",
        "PreferredAuthentications=password",
        "PubkeyAuthentication=no",
    ] {
        argv.push("-o".to_string());
        argv.push(option.to_string());
    }
    argv.push("-o".to_string());
    argv.push(format!("ConnectTimeout={}", cfg.connect_timeout_s));

    argv.push(format!("{}@{}", cfg.user, cfg.host));
    argv.push(remote.to_string());
    argv
}

/// Environment overrides that make non-interactive password auth work. Empty
/// when no askpass is configured (e.g. key auth added later).
#[must_use]
pub fn ssh_env(cfg: &SshConfig) -> Vec<(String, String)> {
    match &cfg.askpass {
        Some(path) => vec![
            ("SSH_ASKPASS".to_string(), path.clone()),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            // Ensure ssh never falls back to prompting on a tty.
            ("DISPLAY".to_string(), ":0".to_string()),
        ],
        None => Vec::new(),
    }
}

/// Executes remote commands on the router through an injected runner.
#[derive(Debug, Clone)]
pub struct SshExec<'r, R: CommandRunner> {
    runner: &'r R,
    cfg: SshConfig,
    /// Wall-clock ceiling for the whole ssh call (>= the ssh connect timeout).
    deadline: Duration,
}

impl<'r, R: CommandRunner> SshExec<'r, R> {
    #[must_use]
    pub fn new(runner: &'r R, cfg: SshConfig, deadline: Duration) -> Self {
        Self {
            runner,
            cfg,
            deadline,
        }
    }

    /// Run `remote` on the router and return its raw output.
    pub fn exec(&self, remote: &str) -> Result<CommandOutput, RunError> {
        let argv = build_remote_argv(&self.cfg, remote);
        let env = ssh_env(&self.cfg);
        self.runner.run(&argv, &env, None, self.deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::{build_remote_argv, ssh_env, SshConfig};

    fn cfg() -> SshConfig {
        SshConfig {
            host: "192.168.1.1".to_string(),
            user: "root".to_string(),
            bind: Some("192.168.1.223".to_string()),
            askpass: Some("/tmp/askpass.sh".to_string()),
            connect_timeout_s: 8,
        }
    }

    #[test]
    fn argv_wraps_ssh_in_setsid_and_targets_the_router() {
        let argv = build_remote_argv(&cfg(), "uptime");
        assert_eq!(argv.first().map(String::as_str), Some("setsid"));
        assert!(argv.iter().any(|a| a == "ssh"));
        assert!(argv.iter().any(|a| a == "root@192.168.1.1"));
        assert_eq!(argv.last().map(String::as_str), Some("uptime"));
    }

    #[test]
    fn argv_includes_bind_and_connect_timeout() {
        let argv = build_remote_argv(&cfg(), "true");
        assert!(argv
            .windows(2)
            .any(|w| w == ["-b".to_string(), "192.168.1.223".to_string()]));
        assert!(argv.iter().any(|a| a == "ConnectTimeout=8"));
    }

    #[test]
    fn argv_omits_bind_when_not_configured() {
        let mut c = cfg();
        c.bind = None;
        let argv = build_remote_argv(&c, "true");
        assert!(!argv.iter().any(|a| a == "-b"));
    }

    #[test]
    fn env_carries_askpass_when_configured() {
        let env = ssh_env(&cfg());
        assert!(env
            .iter()
            .any(|(k, v)| k == "SSH_ASKPASS" && v == "/tmp/askpass.sh"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "SSH_ASKPASS_REQUIRE" && v == "force"));
    }

    #[test]
    fn env_is_empty_without_askpass() {
        let mut c = cfg();
        c.askpass = None;
        assert!(ssh_env(&c).is_empty());
    }
}
