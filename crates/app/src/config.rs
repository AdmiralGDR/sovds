// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Runtime configuration, assembled from the environment with safe defaults.
//!
//! Configuration is validated at this boundary: malformed or missing
//! values fall back to documented defaults rather than propagating garbage into
//! the core. Nothing here is a global — the values are handed explicitly to the
//! composition root.

use sovereign_adapters::ssh::{HostKeyPolicy, PinnedHostKey, SshConfig};
use sovereign_core::domain::LatencyMs;
use sovereign_core::fleet::FleetConfig;
use sovereign_core::fsm::Config as CoreConfig;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// Everything the composition root needs to build and run the supervisor.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub ssh: SshConfig,
    pub telemetry_deadline: Duration,
    pub actuator_deadline: Duration,
    pub ntfy_topic: String,
    pub ntfy_timeout: Duration,
    pub core: CoreConfig,
    /// Adaptive-layer parameters (tuned from real data by default).
    pub fleet: FleetConfig,
    pub interval: Duration,
    /// `None` runs forever (service mode); `Some(n)` stops after n cycles.
    pub budget: Option<u64>,
    /// Where the audit/training records are appended.
    pub log_path: PathBuf,
    /// Rotate the record log once it reaches this size.
    pub log_max_bytes: u64,
    /// How many rotated record logs to keep.
    pub log_generations: u32,
    /// Observe-only mode: run the brain and record, but never touch the router.
    /// Safe by default (least privilege); actuation is opt-in.
    pub dry_run: bool,
}

impl AppConfig {
    /// Read configuration from `SOVEREIGN_*` environment variables, falling back
    /// to defaults tuned for the current router.
    #[must_use]
    pub fn from_env() -> Self {
        let ssh = SshConfig {
            host: var("SOVEREIGN_HOST").unwrap_or_else(|| "192.168.1.1".to_string()),
            user: var("SOVEREIGN_USER").unwrap_or_else(|| "root".to_string()),
            bind: var("SOVEREIGN_BIND"),
            askpass: var("SOVEREIGN_ASKPASS"),
            identity_file: var("SOVEREIGN_SSH_IDENTITY"),
            host_key: host_key_policy(
                var("SOVEREIGN_SSH_INSECURE_ACCEPT_ANY_HOST_KEY").as_deref(),
                var("SOVEREIGN_HOST_KEY").as_deref(),
                var("SOVEREIGN_KNOWN_HOSTS"),
            ),
            connect_timeout_s: parse_u32("SOVEREIGN_SSH_CONNECT_S", 8),
        };
        Self {
            ssh,
            telemetry_deadline: Duration::from_secs(parse_u64("SOVEREIGN_TELEMETRY_S", 15)),
            actuator_deadline: Duration::from_secs(parse_u64("SOVEREIGN_ACTUATOR_S", 30)),
            ntfy_topic: var("SOVEREIGN_NTFY_TOPIC")
                .unwrap_or_else(|| "sovereign-router-alerts".to_string()),
            ntfy_timeout: Duration::from_secs(parse_u64("SOVEREIGN_NTFY_S", 10)),
            core: CoreConfig {
                degraded_above: LatencyMs(parse_u32("SOVEREIGN_DEGRADED_MS", 800)),
                // Default 5 (was 3), chosen by replaying 15h of real telemetry:
                // it filters the transient node_dead probe-failure clusters that
                // shorter hysteresis let through.
                confirm_threshold: u8::try_from(parse_u64("SOVEREIGN_CONFIRM", 5)).unwrap_or(5),
                cooldown_ticks: parse_u64("SOVEREIGN_COOLDOWN", 3),
            },
            fleet: {
                let t = FleetConfig::tuned();
                FleetConfig {
                    weight: parse_u64("SOVEREIGN_FLEET_WEIGHT", t.weight),
                    slack_ms: parse_u64("SOVEREIGN_FLEET_SLACK_MS", t.slack_ms),
                    threshold_ms: parse_u64("SOVEREIGN_FLEET_THRESHOLD_MS", t.threshold_ms),
                    warmup: parse_u32("SOVEREIGN_FLEET_WARMUP", t.warmup),
                    miss_limit: parse_u32("SOVEREIGN_FLEET_MISS_LIMIT", t.miss_limit),
                    slow_factor: parse_u64("SOVEREIGN_FLEET_SLOW_FACTOR", t.slow_factor),
                    min_spread_ms: parse_u64("SOVEREIGN_FLEET_MIN_SPREAD_MS", t.min_spread_ms),
                }
            },
            interval: Duration::from_secs(parse_u64("SOVEREIGN_INTERVAL_S", 30)),
            budget: var("SOVEREIGN_CYCLES").and_then(|s| s.parse::<u64>().ok()),
            // Default matches sovereign-inspect/-replay: one XDG location, never
            // a cwd-relative path (a relative default once sent backfilled
            // records into a stray file the other tools could not see).
            log_path: var("SOVEREIGN_LOG").map_or_else(default_log_path, PathBuf::from),
            log_max_bytes: parse_u64("SOVEREIGN_LOG_MAX_BYTES", 5_000_000),
            log_generations: parse_u32("SOVEREIGN_LOG_GENERATIONS", 5),
            // Safe by default: only an explicit SOVEREIGN_ACTUATE=1 enables actions.
            dry_run: var("SOVEREIGN_ACTUATE").is_none_or(|value| value != "1"),
        }
    }
}

/// Resolve the host-key policy. Precedence: a valid pinned key, then the
/// explicit insecure opt-in (must be exactly `1`), then strict known_hosts
/// checking. A pin always wins over the insecure switch, so leaving the
/// switch set after pinning cannot silently undo the pin. A malformed pinned
/// key does NOT weaken security: it falls back to strict known_hosts checking
/// (never to insecure), which rejects an unknown router loudly.
fn host_key_policy(
    insecure: Option<&str>,
    pinned: Option<&str>,
    known_hosts: Option<String>,
) -> HostKeyPolicy {
    let pin = pinned.and_then(PinnedHostKey::parse);
    if pinned.is_some() && pin.is_none() {
        eprintln!(
            "sovereign: SOVEREIGN_HOST_KEY is not a valid '<type> <base64>' key; \
             ignoring it and using strict known_hosts checking"
        );
    }
    if let Some(key) = pin {
        if insecure == Some("1") {
            eprintln!(
                "sovereign: SOVEREIGN_HOST_KEY is set, so \
                 SOVEREIGN_SSH_INSECURE_ACCEPT_ANY_HOST_KEY is ignored"
            );
        }
        return HostKeyPolicy::Pinned(key);
    }
    if insecure == Some("1") {
        return HostKeyPolicy::InsecureAcceptAny;
    }
    HostKeyPolicy::KnownHosts { file: known_hosts }
}

/// The canonical dataset location on the laptop (XDG data dir), shared as the
/// default by every binary.
fn default_log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}

/// A non-empty environment variable, or `None`.
fn var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

fn parse_u64(key: &str, default: u64) -> u64 {
    var(key)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn parse_u32(key: &str, default: u32) -> u32 {
    var(key)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::host_key_policy;
    use sovereign_adapters::ssh::HostKeyPolicy;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn default_is_strict_known_hosts() {
        assert_eq!(
            host_key_policy(None, None, None),
            HostKeyPolicy::KnownHosts { file: None }
        );
    }

    #[test]
    fn insecure_requires_exact_opt_in() {
        assert_eq!(
            host_key_policy(Some("1"), None, None),
            HostKeyPolicy::InsecureAcceptAny
        );
        assert_ne!(
            host_key_policy(Some("yes"), None, None),
            HostKeyPolicy::InsecureAcceptAny
        );
    }

    #[test]
    fn pinned_key_wins_over_known_hosts() {
        assert!(matches!(
            host_key_policy(None, Some(KEY), Some("/x".to_string())),
            HostKeyPolicy::Pinned(_)
        ));
    }

    #[test]
    fn a_valid_pin_wins_over_the_insecure_switch() {
        assert!(matches!(
            host_key_policy(Some("1"), Some(KEY), None),
            HostKeyPolicy::Pinned(_)
        ));
    }

    #[test]
    fn malformed_pin_falls_back_to_strict_not_insecure() {
        assert_eq!(
            host_key_policy(None, Some("garbage"), Some("/x".to_string())),
            HostKeyPolicy::KnownHosts {
                file: Some("/x".to_string())
            }
        );
    }
}
