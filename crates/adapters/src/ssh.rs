// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! SSH bridge: build the exact `ssh` invocation for running a command on the
//! router, and execute it through a [`CommandRunner`].
//!
//! The OpenSSH client is quarantined behind [`SshConfig`]: the rest of
//! the system never sees a raw command line. Password auth is driven by an
//! askpass helper whose path is passed in the environment, never on the argv, so
//! the secret never appears in a process list.
//!
//! # Host authentication
//!
//! The router's identity is always verified unless the operator explicitly
//! opts out (see [`HostKeyPolicy`]). Without host-key checking anyone able to
//! answer on the router address (ARP/DHCP spoofing, a rogue AP, a hijacked
//! Tailscale name) receives the root password from the askpass helper and gets
//! a root shell on the real router by relaying it. A reflash that rotates the
//! router key now fails *loudly* ([`RunError::HostKeyRejected`]) instead of
//! being silently accepted; the fix is to re-pin the new key.
//!
//! # Client authentication
//!
//! Public-key auth is always offered first. Password auth (via askpass) is only
//! enabled as a fallback when an askpass helper is configured and the host key
//! is verified; in the insecure mode the password is never offered.

use crate::os::{CommandOutput, CommandRunner, RunError};
use std::fmt;
use std::time::Duration;

/// How the router's SSH host key is verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// Strict checking against a `known_hosts` file. `None` uses OpenSSH's
    /// defaults (`~/.ssh/known_hosts` + the global file). Unknown or changed
    /// keys are rejected; nothing is ever auto-added.
    KnownHosts {
        /// Dedicated known_hosts file, e.g. `~/.config/sovereign/known_hosts`.
        file: Option<String>,
    },
    /// Pin one exact public key (`<type> <base64>`), independent of any
    /// known_hosts file. Requires OpenSSH >= 8.5 (`KnownHostsCommand`).
    Pinned(PinnedHostKey),
    /// Legacy behaviour: accept any host key. Only reachable through an explicit
    /// opt-in; vulnerable to man-in-the-middle and therefore never the default.
    InsecureAcceptAny,
}

impl Default for HostKeyPolicy {
    fn default() -> Self {
        Self::KnownHosts { file: None }
    }
}

impl fmt::Display for HostKeyPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KnownHosts { file: None } => f.write_str("strict (default known_hosts)"),
            Self::KnownHosts { file: Some(path) } => write!(f, "strict (known_hosts: {path})"),
            Self::Pinned(key) => write!(f, "pinned ({})", key.key_type()),
            Self::InsecureAcceptAny => f.write_str("INSECURE: any host key accepted"),
        }
    }
}

/// A validated `<type> <base64>` OpenSSH public host key.
///
/// Both halves are restricted to a conservative character set so they can be
/// placed on the `KnownHostsCommand` line without any quoting or token
/// expansion surprises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedHostKey {
    key_type: String,
    key_b64: String,
}

impl PinnedHostKey {
    /// Parse `ssh-ed25519 AAAAC3Nza... [comment]` (the format printed by
    /// `ssh-keyscan`, `ssh-keygen -y` or `dropbearkey -y`). A leading host
    /// field, as in a known_hosts line, is tolerated. Returns `None` when the
    /// input is not a plausible public key.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        // Find the first `<type> <base64>` pair; the type always contains a
        // dash (ssh-ed25519, ecdsa-sha2-nistp256, ssh-rsa, sk-...@openssh.com).
        tokens.windows(2).find_map(|pair| match pair {
            [ty, b64] if valid_key_type(ty) && valid_base64(b64) => Some(Self {
                key_type: (*ty).to_string(),
                key_b64: (*b64).to_string(),
            }),
            _ => None,
        })
    }

    /// The key algorithm, e.g. `ssh-ed25519`.
    #[must_use]
    pub fn key_type(&self) -> &str {
        &self.key_type
    }
}

fn valid_key_type(s: &str) -> bool {
    (3..=64).contains(&s.len())
        && s.contains('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-@.".contains(&b))
}

fn valid_base64(s: &str) -> bool {
    // Real keys start with the base64 of a length-prefixed type ("AAAA").
    s.starts_with("AAAA")
        && (16..=16384).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
}

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
    /// the argv so the password never leaks into `ps`. When set, password auth
    /// is allowed as a *fallback* after public-key auth.
    pub askpass: Option<String>,
    /// Private key used for public-key auth (`ssh -i`). `None` lets ssh use the
    /// agent / default identities.
    pub identity_file: Option<String>,
    /// How the router's host key is verified. Strict by default.
    pub host_key: HostKeyPolicy,
    /// Hard connect timeout handed to ssh itself, in seconds.
    pub connect_timeout_s: u32,
}

fn push_opt(argv: &mut Vec<String>, option: String) {
    argv.push("-o".to_string());
    argv.push(option);
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

    // Host authentication: verify the router unless explicitly told not to.
    match &cfg.host_key {
        HostKeyPolicy::KnownHosts { file } => {
            push_opt(&mut argv, "StrictHostKeyChecking=yes".to_string());
            if let Some(path) = file {
                // Quoted so a path with spaces survives ssh's option parser.
                push_opt(&mut argv, format!("UserKnownHostsFile=\"{path}\""));
            }
        }
        HostKeyPolicy::Pinned(key) => {
            // Only the pinned key is acceptable: ignore every known_hosts file
            // and synthesise a single known_hosts line for the host being
            // looked up (%H). Both key fields are charset-validated.
            push_opt(&mut argv, "StrictHostKeyChecking=yes".to_string());
            push_opt(&mut argv, "UserKnownHostsFile=/dev/null".to_string());
            push_opt(&mut argv, "GlobalKnownHostsFile=/dev/null".to_string());
            push_opt(
                &mut argv,
                format!(
                    "KnownHostsCommand=/usr/bin/env echo %H {} {}",
                    key.key_type, key.key_b64
                ),
            );
        }
        HostKeyPolicy::InsecureAcceptAny => {
            push_opt(&mut argv, "StrictHostKeyChecking=no".to_string());
            push_opt(&mut argv, "UserKnownHostsFile=/dev/null".to_string());
            push_opt(&mut argv, "GlobalKnownHostsFile=/dev/null".to_string());
        }
    }

    // Client authentication: key first, password only as a configured fallback.
    if let Some(identity) = &cfg.identity_file {
        argv.push("-i".to_string());
        argv.push(identity.clone());
        push_opt(&mut argv, "IdentitiesOnly=yes".to_string());
    }
    push_opt(&mut argv, "PubkeyAuthentication=yes".to_string());
    if password_fallback_allowed(cfg) {
        push_opt(
            &mut argv,
            "PreferredAuthentications=publickey,password".to_string(),
        );
    } else {
        // No askpass: never prompt, fail fast.
        push_opt(&mut argv, "PreferredAuthentications=publickey".to_string());
        push_opt(&mut argv, "PasswordAuthentication=no".to_string());
        push_opt(&mut argv, "BatchMode=yes".to_string());
    }
    push_opt(
        &mut argv,
        format!("ConnectTimeout={}", cfg.connect_timeout_s),
    );

    argv.push(format!("{}@{}", cfg.user, cfg.host));
    argv.push(remote.to_string());
    argv
}

/// Password auth is only offered to a host whose key was verified. With
/// [`HostKeyPolicy::InsecureAcceptAny`] the askpass helper is ignored, so an
/// impostor on the router address can never collect the root password.
fn password_fallback_allowed(cfg: &SshConfig) -> bool {
    cfg.askpass.is_some() && cfg.host_key != HostKeyPolicy::InsecureAcceptAny
}

/// Environment overrides that make non-interactive password auth work. Empty
/// when no askpass is configured (pure key auth) or when the host key is not
/// verified.
#[must_use]
pub fn ssh_env(cfg: &SshConfig) -> Vec<(String, String)> {
    if !password_fallback_allowed(cfg) {
        return Vec::new();
    }
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

/// True when ssh refused to connect because the host key was unknown or did
/// not match. OpenSSH exits 255 and always prints "Host key verification
/// failed." in that case (plus a banner for a *changed* key).
#[must_use]
pub fn is_host_key_rejection(output: &CommandOutput) -> bool {
    if output.code != Some(255) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
}

/// Human-readable explanation for a host-key rejection, with the remedy.
#[must_use]
pub fn host_key_rejection_message(cfg: &SshConfig) -> String {
    format!(
        "sovereign: SSH host key for {host} was REJECTED (policy: {policy}). \
         Refusing to send credentials: this is either a router reflash (new key) \
         or a man-in-the-middle. Verify the key on the router console \
         (e.g. `dropbearkey -y -f /etc/dropbear/dropbear_ed25519_host_key`), then \
         pin it with SOVEREIGN_HOST_KEY='<type> <base64>' or add it to the \
         known_hosts file (SOVEREIGN_KNOWN_HOSTS).",
        host = cfg.host,
        policy = cfg.host_key,
    )
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
    ///
    /// # Errors
    /// [`RunError::HostKeyRejected`] when the router's identity could not be
    /// verified (a diagnostic with the remedy is written to stderr), otherwise
    /// whatever the runner reports.
    pub fn exec(&self, remote: &str) -> Result<CommandOutput, RunError> {
        let argv = build_remote_argv(&self.cfg, remote);
        let env = ssh_env(&self.cfg);
        let output = self.runner.run(&argv, &env, None, self.deadline)?;
        if is_host_key_rejection(&output) {
            eprintln!("{}", host_key_rejection_message(&self.cfg));
            return Err(RunError::HostKeyRejected);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_remote_argv, is_host_key_rejection, ssh_env, HostKeyPolicy, PinnedHostKey, SshConfig,
        SshExec,
    };
    use crate::os::{CommandOutput, CommandRunner, RunError};
    use std::time::Duration;

    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn cfg() -> SshConfig {
        SshConfig {
            host: "192.168.1.1".to_string(),
            user: "root".to_string(),
            bind: Some("192.168.1.223".to_string()),
            askpass: Some("/tmp/askpass.sh".to_string()),
            identity_file: None,
            host_key: HostKeyPolicy::default(),
            connect_timeout_s: 8,
        }
    }

    fn has(argv: &[String], s: &str) -> bool {
        argv.iter().any(|a| a == s)
    }

    #[test]
    fn argv_wraps_ssh_in_setsid_and_targets_the_router() {
        let argv = build_remote_argv(&cfg(), "uptime");
        assert_eq!(argv.first().map(String::as_str), Some("setsid"));
        assert!(has(&argv, "ssh"));
        assert!(has(&argv, "root@192.168.1.1"));
        assert_eq!(argv.last().map(String::as_str), Some("uptime"));
    }

    #[test]
    fn argv_includes_bind_and_connect_timeout() {
        let argv = build_remote_argv(&cfg(), "true");
        assert!(argv
            .windows(2)
            .any(|w| w == ["-b".to_string(), "192.168.1.223".to_string()]));
        assert!(has(&argv, "ConnectTimeout=8"));
    }

    #[test]
    fn argv_omits_bind_when_not_configured() {
        let mut c = cfg();
        c.bind = None;
        let argv = build_remote_argv(&c, "true");
        assert!(!has(&argv, "-b"));
    }

    #[test]
    fn default_policy_is_strict_and_never_disables_known_hosts() {
        let argv = build_remote_argv(&cfg(), "true");
        assert!(has(&argv, "StrictHostKeyChecking=yes"));
        assert!(!has(&argv, "StrictHostKeyChecking=no"));
        assert!(!argv.iter().any(|a| a.contains("/dev/null")));
    }

    #[test]
    fn known_hosts_file_is_passed_quoted() {
        let mut c = cfg();
        c.host_key = HostKeyPolicy::KnownHosts {
            file: Some("/home/u/.config/sovereign/known_hosts".to_string()),
        };
        let argv = build_remote_argv(&c, "true");
        assert!(has(
            &argv,
            "UserKnownHostsFile=\"/home/u/.config/sovereign/known_hosts\""
        ));
        assert!(has(&argv, "StrictHostKeyChecking=yes"));
    }

    #[test]
    fn pinned_key_ignores_known_hosts_and_injects_exact_key() {
        let mut c = cfg();
        let key = PinnedHostKey::parse(ED25519);
        assert!(key.is_some());
        if let Some(key) = key {
            c.host_key = HostKeyPolicy::Pinned(key);
        }
        let argv = build_remote_argv(&c, "true");
        assert!(has(&argv, "StrictHostKeyChecking=yes"));
        assert!(has(&argv, "UserKnownHostsFile=/dev/null"));
        assert!(has(&argv, "GlobalKnownHostsFile=/dev/null"));
        assert!(has(
            &argv,
            &format!("KnownHostsCommand=/usr/bin/env echo %H {ED25519}")
        ));
    }

    #[test]
    fn insecure_mode_only_when_explicitly_selected() {
        let mut c = cfg();
        c.host_key = HostKeyPolicy::InsecureAcceptAny;
        let argv = build_remote_argv(&c, "true");
        assert!(has(&argv, "StrictHostKeyChecking=no"));
    }

    #[test]
    fn insecure_mode_never_offers_the_password() {
        let mut c = cfg();
        c.host_key = HostKeyPolicy::InsecureAcceptAny;
        let argv = build_remote_argv(&c, "true");
        assert!(has(&argv, "PreferredAuthentications=publickey"));
        assert!(has(&argv, "PasswordAuthentication=no"));
        assert!(has(&argv, "BatchMode=yes"));
        assert!(!has(&argv, "PreferredAuthentications=publickey,password"));
        assert!(ssh_env(&c).is_empty());
    }

    #[test]
    fn pinned_key_parsing_accepts_real_formats_and_rejects_junk() {
        assert!(PinnedHostKey::parse(ED25519).is_some());
        assert!(PinnedHostKey::parse(&format!("{ED25519} root@OpenWrt")).is_some());
        assert!(PinnedHostKey::parse(&format!("192.168.1.1 {ED25519}")).is_some());
        assert_eq!(
            PinnedHostKey::parse(ED25519).map(|k| k.key_type().to_string()),
            Some("ssh-ed25519".to_string())
        );
        assert!(PinnedHostKey::parse("").is_none());
        assert!(PinnedHostKey::parse("SHA256:abcdef").is_none());
        assert!(PinnedHostKey::parse("ssh-ed25519 AAAA;rm -rf /").is_none());
        assert!(PinnedHostKey::parse("ssh-ed25519 $(id)AAAAAAAAAAAAAAAAAAAA").is_none());
        assert!(PinnedHostKey::parse("Ssh-Ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMq").is_none());
    }

    #[test]
    fn key_auth_is_preferred_and_password_is_only_a_fallback() {
        let argv = build_remote_argv(&cfg(), "true");
        assert!(has(&argv, "PubkeyAuthentication=yes"));
        assert!(has(&argv, "PreferredAuthentications=publickey,password"));
        assert!(!has(&argv, "PubkeyAuthentication=no"));
    }

    #[test]
    fn without_askpass_password_auth_is_disabled_and_batch_mode_on() {
        let mut c = cfg();
        c.askpass = None;
        c.identity_file = Some("/home/u/.ssh/id_router".to_string());
        let argv = build_remote_argv(&c, "true");
        assert!(argv
            .windows(2)
            .any(|w| w == ["-i".to_string(), "/home/u/.ssh/id_router".to_string()]));
        assert!(has(&argv, "IdentitiesOnly=yes"));
        assert!(has(&argv, "PreferredAuthentications=publickey"));
        assert!(has(&argv, "PasswordAuthentication=no"));
        assert!(has(&argv, "BatchMode=yes"));
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

    fn out(code: Option<i32>, stderr: &str) -> CommandOutput {
        CommandOutput {
            code,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn host_key_rejection_is_detected() {
        assert!(is_host_key_rejection(&out(
            Some(255),
            "No ED25519 host key is known for 192.168.1.1 and you have requested strict checking.\r\nHost key verification failed.\r\n"
        )));
        assert!(is_host_key_rejection(&out(
            Some(255),
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@"
        )));
        assert!(!is_host_key_rejection(&out(
            Some(255),
            "Connection refused"
        )));
        assert!(!is_host_key_rejection(&out(
            Some(1),
            "Host key verification failed."
        )));
    }

    struct FakeRunner(CommandOutput);

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _argv: &[String],
            _env: &[(String, String)],
            _stdin: Option<&[u8]>,
            _timeout: Duration,
        ) -> Result<CommandOutput, RunError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn exec_surfaces_host_key_rejection_as_a_distinct_error() {
        let runner = FakeRunner(out(Some(255), "Host key verification failed.\n"));
        let ssh = SshExec::new(&runner, cfg(), Duration::from_secs(1));
        assert_eq!(ssh.exec("true"), Err(RunError::HostKeyRejected));
    }

    #[test]
    fn exec_passes_through_normal_output() {
        let runner = FakeRunner(out(Some(0), ""));
        let ssh = SshExec::new(&runner, cfg(), Duration::from_secs(1));
        assert_eq!(ssh.exec("true").map(|o| o.code), Ok(Some(0)));
    }
}
