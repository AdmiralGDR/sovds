// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Composition root.
//!
//! The one place where concrete adapters are injected into the pure core and the
//! bounded loop is started. All I/O types are constructed here and nowhere else
//! (I/O lives only at the outermost layer).

mod config;
mod supervisor;

use config::AppConfig;
use sovereign_adapters::actuator::{NullActuator, SshActuator};
use sovereign_adapters::notify::NtfyNotifier;
use sovereign_adapters::os::SystemRunner;
use sovereign_adapters::recorder::FileRecorder;
use sovereign_adapters::ssh::SshExec;
use sovereign_adapters::telemetry::SshTelemetry;
use sovereign_core::ports::{Actuator, Notifier, TelemetrySource};
use sovereign_core::record::{Outcome, Record};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use supervisor::Supervisor;

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
    let notifier = NtfyNotifier::new(&runner, cfg.ntfy_topic.clone(), cfg.ntfy_timeout);

    // Durable audit/training trail. Recording failures are logged and shrugged
    // off — the trail is a courtesy to the future, never load-bearing.
    let recorder = FileRecorder::new(cfg.log_path.clone(), cfg.log_max_bytes, cfg.log_generations);

    let mode = if cfg.dry_run {
        "observe-only (dry-run)"
    } else {
        "LIVE (actuation on)"
    };
    eprintln!(
        "sovereign: {mode} — {}@{} (host key: {}) every {}s (budget: {}), recording to {}",
        cfg.ssh.user,
        cfg.ssh.host,
        cfg.ssh.host_key,
        cfg.interval.as_secs(),
        cfg.budget
            .map_or_else(|| "service".to_string(), |n| n.to_string()),
        cfg.log_path.display(),
    );

    // The actuator type is chosen once, here: a NullActuator in dry-run cannot
    // touch the router at all (safety by construction), an SshActuator can. Both
    // arms move `telemetry`/`notifier`, which is sound because only one runs.
    if cfg.dry_run {
        supervise(
            Supervisor::new(telemetry, NullActuator, notifier, cfg.core, cfg.fleet),
            cfg.budget,
            cfg.interval,
            &recorder,
        );
    } else {
        let actuator = SshActuator::new(SshExec::new(
            &runner,
            cfg.ssh.clone(),
            cfg.actuator_deadline,
        ));
        supervise(
            Supervisor::new(telemetry, actuator, notifier, cfg.core, cfg.fleet),
            cfg.budget,
            cfg.interval,
            &recorder,
        );
    }
    ExitCode::SUCCESS
}

/// Run the supervision loop to completion, logging and recording each cycle.
/// Generic over the actuator so dry-run and live share one code path.
fn supervise<T, A, N>(
    mut supervisor: Supervisor<T, A, N>,
    budget: Option<u64>,
    interval: Duration,
    recorder: &FileRecorder,
) where
    T: TelemetrySource,
    A: Actuator,
    N: Notifier,
{
    let observe = |record: &Record| {
        log_step(record);
        if let Err(err) = recorder.append(record) {
            eprintln!("sovereign: record dropped ({err:?})");
        }
    };
    let completed = supervisor.run(budget, interval, unix_now, std::thread::sleep, observe);
    eprintln!(
        "sovereign: completed {completed} cycle(s), final state {:?}",
        supervisor.state()
    );
}

/// Current wall-clock time as Unix seconds (0 if the clock is before the epoch,
/// which cannot happen in practice). This is the only place a real clock is read.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Apply command-line overrides on top of the environment-derived config.
/// Unknown flags are ignored (forgiving boundary).
fn apply_args<I: Iterator<Item = String>>(cfg: &mut AppConfig, mut args: I) {
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => cfg.budget = Some(1),
            "--dry-run" => cfg.dry_run = true,
            "--live" => cfg.dry_run = false,
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
fn log_step(record: &Record) {
    let verdict = match record.annot.outcome {
        Outcome::Verified => " (verified)",
        Outcome::Unverified => " (UNVERIFIED)",
        Outcome::Simulated => " (dry-run)",
        Outcome::NotActed => "",
    };
    let source = if record.probed { "ok" } else { "degraded" };
    eprintln!(
        "  [{source}] {:?} -> {:?}{verdict}",
        record.annot.reason, record.annot.action
    );
}
