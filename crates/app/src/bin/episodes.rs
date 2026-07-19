//! `sovereign-episodes` — segment the collected records into fault episodes and
//! report the dataset's training-readiness per class.
//!
//! Rows are not the unit that matters; independent fault episodes are (the principle).
//! This is the honest answer to "how ready is the dataset to train a model?": a
//! healthy network yields few, so the rare classes converge in episodes very
//! slowly, and this prints exactly how slowly. `--list` dumps every episode.

use sovds::episode::{segment, Episode, EpisodeEnd, SegConfig};
use sovds::injection;
use sovds::readiness::{summarize, ClassReadiness};
use sovereign_adapters::dataset;
use sovereign_core::failure::FailureClass;
use sovereign_core::record::Source;
use sovereign_core::routerdomain::RouterDomain;
use std::path::PathBuf;
use std::process::ExitCode;

type RouterEpisode = Episode<RouterDomain>;
type RouterReadiness = ClassReadiness<RouterDomain>;
type RouterRecord = sovereign_core::record::Record;

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

    let episodes = segment::<RouterDomain>(&corpus.records, SegConfig::new());
    let summary = summarize(&episodes);

    if want_list() {
        print_list(&episodes);
        println!();
    }
    print_summary(&summary, episodes.len());
    print_injection_scorecard(&corpus.records);
    print_injection_safety(&corpus.records);

    eprintln!(
        "\n(from {} record(s) in {} file(s); {} line(s) skipped)",
        corpus.records.len(),
        corpus.files,
        corpus.skipped
    );
    ExitCode::SUCCESS
}

fn print_summary(summary: &[RouterReadiness], total: usize) {
    println!("Fault episodes (the training-readiness unit — independent incidents, not rows)\n");
    println!(
        "{:<14} {:>8} {:>10} {:>10} {:>11}  readiness",
        "class", "episodes", "recovered", "cycles", "median"
    );
    println!("{}", "-".repeat(70));
    for c in summary {
        let ready = if c.count >= READY_TARGET {
            "ok".to_string()
        } else {
            format!("{}/{READY_TARGET}", c.count)
        };
        println!(
            "{:<14} {:>8} {:>10} {:>10} {:>9}s  {ready}",
            class_label(c.class),
            c.count,
            c.recovered,
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

/// Injection ground-truth vs. diagnosis: did the rules detect the faults we
/// induced on purpose? Only shown once labelled injection data exists.
fn print_injection_scorecard(records: &[RouterRecord]) {
    let stats: Vec<_> = injection::accuracy(records).collect();
    if stats.is_empty() {
        return;
    }
    println!("\nInjection detection — per window (ground truth `inject=` vs diagnosed `class=`)\n");
    println!(
        "{:<14} {:>8} {:>9} {:>7} {:>7} {:>6}",
        "injected", "windows", "detected", "missed", "silent", "rate"
    );
    println!("{}", "-".repeat(56));
    for s in &stats {
        // Rate is over windows where the fault actually manifested (detected+missed);
        // silent windows (dry-run / cadence miss) are excluded, not counted against.
        let manifested = s.detected.saturating_add(s.missed);
        let rate = s
            .detected
            .saturating_mul(100)
            .checked_div(manifested)
            .map_or_else(|| "n/a".to_string(), |r| format!("{r}%"));
        println!(
            "{:<14} {:>8} {:>9} {:>7} {:>7} {:>6}",
            class_label(s.class),
            s.windows,
            s.detected,
            s.missed,
            s.silent,
            rate
        );
    }
    println!(
        "  (rate = detected / windows where the fault manifested; silent = dry-run/cadence miss)"
    );
}

/// Rollback safety for the autonomous injector: did every induced fault recover?
/// Measured over injection windows. Only shown once injections exist.
fn print_injection_safety(records: &[RouterRecord]) {
    let s = injection::safety(records);
    if s.windows == 0 {
        return;
    }
    let mut line = format!(
        "\nInjection rollback safety: {}/{} windows auto-recovered",
        s.recovered, s.windows
    );
    if s.pending > 0 {
        line.push_str(&format!(", {} pending (in flight)", s.pending));
    }
    println!("{line}.");
    if s.stuck > 0 {
        println!(
            "  ⚠ {} injection window(s) never returned to health — check the router \
             (sovereign-inject --clear) and the failsafes.",
            s.stuck
        );
    }
}

fn print_list(episodes: &[RouterEpisode]) {
    println!(
        "{:<14} {:>6} {:>7} {:>5} {:<10} {:<8} injected",
        "class", "tick0", "dur(s)", "cycl", "end", "src"
    );
    println!("{}", "-".repeat(66));
    for e in episodes {
        let injected = match e.injected {
            Some(c) => class_label(c),
            None => "-",
        };
        println!(
            "{:<14} {:>6} {:>7} {:>5} {:<10} {:<8} {injected}",
            class_label(e.class),
            e.start_tick.0,
            e.duration_secs(),
            e.records,
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
