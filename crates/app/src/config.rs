//! Runtime configuration, assembled from the environment with safe defaults.
//!
//! Configuration is validated at this boundary (the principle): malformed or missing
//! values fall back to documented defaults rather than propagating garbage into
//! the core. Nothing here is a global — the values are handed explicitly to the
//! composition root.

use sovereign_adapters::ssh::SshConfig;
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
    /// Safe by default (the principle: least privilege); actuation is opt-in.
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
            log_path: var("SOVEREIGN_LOG")
                .map_or_else(|| PathBuf::from("sovereign-records.log"), PathBuf::from),
            log_max_bytes: parse_u64("SOVEREIGN_LOG_MAX_BYTES", 5_000_000),
            log_generations: parse_u32("SOVEREIGN_LOG_GENERATIONS", 5),
            // Safe by default: only an explicit SOVEREIGN_ACTUATE=1 enables actions.
            dry_run: var("SOVEREIGN_ACTUATE").is_none_or(|value| value != "1"),
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
