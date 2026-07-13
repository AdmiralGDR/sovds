//! Composition root.
//!
//! The one place where concrete adapters are injected into the pure core and the
//! bounded loop is started. All I/O types are constructed here and nowhere else
//! (the principle: I/O lives only at the outermost layer).

mod config;
mod supervisor;

use config::AppConfig;
use sovereign_adapters::actuator::SshActuator;
use sovereign_adapters::notify::NtfyNotifier;
use sovereign_adapters::os::SystemRunner;
use sovereign_adapters::ssh::SshExec;
use sovereign_adapters::telemetry::SshTelemetry;
use std::process::ExitCode;
use std::time::Duration;
use supervisor::{StepReport, Supervisor};

fn main() -> ExitCode {
    let mut cfg = AppConfig::from_env();
    apply_args(&mut cfg, std::env::args().skip(1));

    // The OS bridge; every adapter borrows it. It outlives the supervisor.
    let runner = SystemRunner;
    let telemetry = SshTelemetry::new(SshExec::new(
        &runner,
        cfg.ssh.clone(),
        cfg.telemetry_deadline,
    ));
    let actuator = SshActuator::new(SshExec::new(
        &runner,
        cfg.ssh.clone(),
        cfg.actuator_deadline,
    ));
    let notifier = NtfyNotifier::new(&runner, cfg.ntfy_topic.clone(), cfg.ntfy_timeout);

    let mut supervisor = Supervisor::new(telemetry, actuator, notifier, cfg.core);

    eprintln!(
        "sovereign: supervising {}@{} every {}s (budget: {})",
        cfg.ssh.user,
        cfg.ssh.host,
        cfg.interval.as_secs(),
        cfg.budget
            .map_or_else(|| "service".to_string(), |n| n.to_string()),
    );

    let completed = supervisor.run(cfg.budget, cfg.interval, std::thread::sleep, log_step);

    eprintln!(
        "sovereign: completed {completed} cycle(s), final state {:?}",
        supervisor.state()
    );
    ExitCode::SUCCESS
}

/// Apply command-line overrides on top of the environment-derived config.
/// Unknown flags are ignored (forgiving boundary, the principle).
fn apply_args<I: Iterator<Item = String>>(cfg: &mut AppConfig, mut args: I) {
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => cfg.budget = Some(1),
            "--cycles" => {
                if let Some(value) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    cfg.budget = Some(value);
                }
            }
            "--interval" => {
                if let Some(value) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    cfg.interval = Duration::from_secs(value);
                }
            }
            _ => {}
        }
    }
}

/// One line per cycle, on stderr, in a log-friendly shape.
fn log_step(report: &StepReport) {
    let verdict = match report.acted {
        Some(Ok(())) => " (verified)",
        Some(Err(_)) => " (UNVERIFIED)",
        None => "",
    };
    let source = if report.probed { "ok" } else { "degraded" };
    eprintln!(
        "  [{source}] {:?} -> {:?}{verdict}",
        report.reason, report.action
    );
}
