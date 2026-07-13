//! Runtime configuration, assembled from the environment with safe defaults.
//!
//! Configuration is validated at this boundary (the principle): malformed or missing
//! values fall back to documented defaults rather than propagating garbage into
//! the core. Nothing here is a global — the values are handed explicitly to the
//! composition root.

use sovereign_adapters::ssh::SshConfig;
use sovereign_core::domain::LatencyMs;
use sovereign_core::fsm::Config as CoreConfig;
use std::env;
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
    pub interval: Duration,
    /// `None` runs forever (service mode); `Some(n)` stops after n cycles.
    pub budget: Option<u64>,
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
                confirm_threshold: u8::try_from(parse_u64("SOVEREIGN_CONFIRM", 3)).unwrap_or(3),
                cooldown_ticks: parse_u64("SOVEREIGN_COOLDOWN", 3),
            },
            interval: Duration::from_secs(parse_u64("SOVEREIGN_INTERVAL_S", 30)),
            budget: var("SOVEREIGN_CYCLES").and_then(|s| s.parse::<u64>().ok()),
        }
    }
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
