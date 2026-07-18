//! `sovereign-episodes` — segment the collected records into fault episodes and
//! report the dataset's training-readiness per class.
//!
//! Rows are not the unit that matters; independent fault episodes are (the principle).
//! This is the honest answer to "how ready is the dataset to train a model?": a
//! healthy network yields few, so the rare classes converge in episodes very
//! slowly, and this prints exactly how slowly. `--list` dumps every episode.

use sovereign_adapters::dataset;
use sovereign_adapters::episodes::{segment, summarize, ClassEpisodes};
use sovereign_core::episode::{Episode, EpisodeEnd, SegConfig};
use sovereign_core::failure::FailureClass;
use sovereign_core::record::Source;
use std::path::PathBuf;
use std::process::ExitCode;

/// A rough, honest target for independent episodes per class before a supervised
/// model of that class is anything but a guess. Not a promise — a yardstick.
const READY_TARGET: usize = 20;

fn main() -> ExitCode {
    let path = log_path();
    let generations = std::env::var("SOVEREIGN_LOG_GENERATIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);

    let corpus = dataset::load(&path, generations);
    if corpus.records.is_empty() {
        eprintln!(
            "sovereign-episodes: no records at {} ({} file(s), {} skipped)",
            path.display(),
            corpus.files,
            corpus.skipped
        );
        return ExitCode::FAILURE;
    }

    let episodes = segment(&corpus.records, SegConfig::new());
    let summary = summarize(&episodes);

    if want_list() {
        print_list(&episodes);
        println!();
    }
    print_summary(&summary, episodes.len());

    eprintln!(
        "\n(from {} record(s) in {} file(s); {} line(s) skipped)",
        corpus.records.len(),
        corpus.files,
        corpus.skipped
    );
    ExitCode::SUCCESS
}

fn print_summary(summary: &[ClassEpisodes], total: usize) {
    println!("Fault episodes (the training-readiness unit — independent incidents, not rows)\n");
    println!(
        "{:<14} {:>8} {:>10} {:>8} {:>10} {:>11}  readiness",
        "class", "episodes", "recovered", "acted", "cycles", "median"
    );
    println!("{}", "-".repeat(78));
    for c in summary {
        let ready = if c.count >= READY_TARGET {
            "ok".to_string()
        } else {
            format!("{}/{READY_TARGET}", c.count)
        };
        println!(
            "{:<14} {:>8} {:>10} {:>8} {:>10} {:>9}s  {ready}",
            class_label(c.class),
            c.count,
            c.recovered,
            c.with_action,
            c.total_records,
            c.median_secs,
        );
    }
    let converged = summary.iter().filter(|c| c.count >= READY_TARGET).count();
    println!(
        "\n{total} fault episode(s) total; {converged}/{} classes at the ~{READY_TARGET}-episode yardstick.",
        summary.len()
    );
    if converged < summary.len() {
        println!(
            "Passive collection converges slowly for rare classes — the scenario suite \
             (sovereign-scenario --seed) seeds the empty ones."
        );
    }
}

fn print_list(episodes: &[Episode]) {
    println!(
        "{:<14} {:>6} {:>7} {:>5} {:>15} {:<8} {:<8} injected",
        "class", "tick0", "dur(s)", "acts", "peak-action", "end", "src"
    );
    println!("{}", "-".repeat(88));
    for e in episodes {
        let injected = match e.injected {
            Some(c) => class_label(c),
            None => "-",
        };
        println!(
            "{:<14} {:>6} {:>7} {:>5} {:>15} {:<8} {:<8} {injected}",
            class_label(e.class),
            e.start_tick.0,
            e.duration_secs(),
            e.actions,
            action_label(e.peak_action),
            end_label(e.end),
            source_label(e.source),
        );
    }
}

/// Resolve the log path: first CLI arg that is not a flag, else `$SOVEREIGN_LOG`,
/// else the default XDG location.
fn log_path() -> PathBuf {
    if let Some(arg) = std::env::args().skip(1).find(|a| !a.starts_with("--")) {
        return PathBuf::from(arg);
    }
    if let Ok(env) = std::env::var("SOVEREIGN_LOG") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}

fn want_list() -> bool {
    std::env::args().skip(1).any(|a| a == "--list")
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

fn end_label(e: EpisodeEnd) -> &'static str {
    match e {
        EpisodeEnd::Recovered => "recovered",
        EpisodeEnd::Transitioned => "transition",
        EpisodeEnd::RunRestart => "restart",
        EpisodeEnd::Censored => "ongoing",
    }
}

fn source_label(s: Source) -> &'static str {
    match s {
        Source::Live => "live",
        Source::Backfill => "backfill",
        Source::Synthetic => "synth",
    }
}

fn action_label(a: sovereign_core::fsm::Action) -> &'static str {
    use sovereign_core::fsm::Action;
    match a {
        Action::Observe => "observe",
        Action::SwitchNode => "switch_node",
        Action::RestartSingbox => "restart_singbox",
        Action::BounceWan => "bounce_wan",
        Action::FailoverBackup => "failover_backup",
        Action::Reboot => "reboot",
    }
}
