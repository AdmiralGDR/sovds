//! `sovereign-scenario` — replay the canonical fault-scenario suite through the
//! live decision pipeline and report how the rules diagnose and remediate each
//! fault class, including the ones a healthy network never produces.
//!
//! This is the "metrics before theories" instrument for the parts of the decision
//! space the passive dataset cannot reach (the principle): `wan_down`, `singbox_dead`
//! and `dns_fail` have ~zero real samples, so this is the only way to know whether
//! the rules would even diagnose them. It touches nothing on the router — the
//! episodes are deterministic laboratory telemetry ([`sovereign_core::scenario`]).
//!
//! Default: print the accuracy matrix. `--seed <path>`: also append every frame as
//! a `src=synth` record to `<path>` (a *separate* file — the observed dataset stays
//! pure; a trainer combines them explicitly).

use sovereign_adapters::recorder::FileRecorder;
use sovereign_core::domain::Tick;
use sovereign_core::failure::FailureClass;
use sovereign_core::fsm::Action;
use sovereign_core::record::{Outcome, Record, RouterAnnot, Source};
use sovereign_core::scenario::{self, ScenarioReport};
use std::path::PathBuf;
use std::process::ExitCode;

// Shared with the other binaries so the tool measures the *live* configuration
// (env overrides included). Only `core`/`fleet`/`log_*` are read here.
#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
use config::AppConfig;

fn main() -> ExitCode {
    let app = AppConfig::from_env();

    if let Some(name) = trace_name() {
        return trace(&app, &name);
    }

    let seed = seed_path();

    let recorder = seed
        .as_ref()
        .map(|p| FileRecorder::new(p.clone(), app.log_max_bytes, app.log_generations));
    let mut seeded = 0usize;

    let mut reports: Vec<ScenarioReport> = Vec::new();
    for scenario in &scenario::CATALOG {
        let report = match recorder.as_ref() {
            Some(rec) => scenario::run(scenario, &app.core, app.fleet, |i, tel, cycle| {
                let record = Record {
                    tick: Tick(u64::from(i)),
                    // Synthetic frames have no wall-clock origin; 0 == unknown.
                    wall_time: 0,
                    probed: true,
                    obs: *tel,
                    class: cycle.failure,
                    annot: RouterAnnot {
                        verdict: cycle.verdict,
                        state: cycle.next_state,
                        reason: cycle.reason,
                        action: cycle.action,
                        // Laboratory episodes are never actuated.
                        outcome: Outcome::NotActed,
                    },
                    source: Source::Synthetic,
                    // Synthetic frames aren't a live injection; `src=synth` already
                    // sets them apart, so the injection label stays clear.
                    label: None,
                };
                if rec.append(&record).is_ok() {
                    seeded = seeded.saturating_add(1);
                }
            }),
            None => scenario::evaluate(scenario, &app.core, app.fleet),
        };
        reports.push(report);
    }

    print_matrix(&reports);

    match &seed {
        Some(path) => eprintln!(
            "\nseeded {seeded} synthetic record(s) → {} (src=synth; kept out of the observed log)",
            path.display()
        ),
        None => eprintln!("\n(run with --seed <path> to append these frames as src=synth records)"),
    }
    ExitCode::SUCCESS
}

/// Render the per-scenario accuracy matrix.
fn print_matrix(reports: &[ScenarioReport]) {
    println!(
        "{:<17} {:<14} {:<15} {:<18} {:>5} {:>4}",
        "scenario", "class", "detect", "first-action", "warm", "recv"
    );
    println!("{}", "-".repeat(78));
    for r in reports {
        let pct = r
            .detected
            .saturating_mul(100)
            .checked_div(r.fault_frames)
            .unwrap_or(0);
        let detect = match r.first_detect {
            Some(at) => format!("{}/{} @{at} ({pct}%)", r.detected, r.fault_frames),
            None => format!("{}/{} ({pct}%)", r.detected, r.fault_frames),
        };
        let action = match r.action_at {
            Some(at) => format!("{} @{at}", action_label(r.first_action)),
            None => "observe".to_string(),
        };
        // Inline notes for the informative outliers: a hard false positive (acting
        // in warmup) vs. the softer recovery-lag over-actuation vs. the blackhole
        // blind spot.
        let note = if r.warmup_actions > 0 {
            "  ← FALSE POSITIVE (acts before fault)"
        } else if r.name == "wan_blackhole" && r.detected == 0 {
            "  ← GAP: classify ignores wanok"
        } else if r.recovery_actions > 0 {
            "  ← recovery-lag (latched adaptive alarm)"
        } else {
            ""
        };
        println!(
            "{:<17} {:<14} {:<15} {:<18} {:>5} {:>4}{note}",
            r.name,
            class_label(r.fault_class),
            detect,
            action,
            r.warmup_actions,
            r.recovery_actions,
        );
    }
}

/// Dump every frame of one scenario: index, phase, classification, path verdict,
/// and action. The debugger for a scenario's behaviour.
fn trace(app: &AppConfig, name: &str) -> ExitCode {
    let Some(scenario) = scenario::CATALOG.iter().find(|s| s.name == name) else {
        eprintln!("sovereign-scenario: no scenario named {name}");
        return ExitCode::FAILURE;
    };
    println!(
        "{:>3}  {:<8} {:<14} {:<10} action",
        "i", "phase", "class", "verdict"
    );
    scenario::run(scenario, &app.core, app.fleet, |i, tel, cycle| {
        let phase = if scenario::in_fault_frame(i) {
            "FAULT"
        } else {
            "healthy"
        };
        let lat = match tel.tunnel {
            sovereign_core::domain::Tunnel::Healthy { latency }
            | sovereign_core::domain::Tunnel::Degraded { latency } => latency.0,
            _ => 0,
        };
        println!(
            "{i:>3}  {phase:<8} {:<14} {:<10} {} (lat={lat})",
            class_label(cycle.failure),
            verdict_label(cycle.verdict),
            action_label(cycle.action),
        );
    });
    ExitCode::SUCCESS
}

fn verdict_label(v: sovereign_core::fleet::NodeVerdict) -> &'static str {
    use sovereign_core::fleet::NodeVerdict as V;
    match v {
        V::Warming => "warming",
        V::Healthy => "healthy",
        V::Slow => "slow",
        V::Throttled => "throttled",
        V::Down => "down",
    }
}

/// Parse `--trace <name>` from the argument list.
fn trace_name() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--trace=") {
            return Some(rest.to_string());
        }
        if arg == "--trace" {
            return args.next();
        }
    }
    None
}

/// Parse `--seed <path>` / `--seed=<path>` from the argument list.
fn seed_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--seed=") {
            return Some(PathBuf::from(rest));
        }
        if arg == "--seed" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

fn class_label(c: FailureClass) -> &'static str {
    match c {
        FailureClass::None => "-",
        FailureClass::WanDown => "wan_down",
        FailureClass::SingboxDead => "singbox_dead",
        FailureClass::NodeDead => "node_dead",
        FailureClass::NodeDegraded => "node_degraded",
        FailureClass::DnsFail => "dns_fail",
    }
}

fn action_label(a: Action) -> &'static str {
    match a {
        Action::Observe => "observe",
        Action::SwitchNode => "switch_node",
        Action::RestartSingbox => "restart_singbox",
        Action::BounceWan => "bounce_wan",
        Action::FailoverBackup => "failover_backup",
        Action::Reboot => "reboot",
    }
}
