// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `sovereign-anomaly` — unsupervised scan for what the rules missed.
//!
//! The rules diagnose *known* faults. This scores every record against an online
//! baseline of its raw signals and surfaces the ones that look statistically
//! unusual — especially records the rules called healthy (`class=none`) yet that
//! sit far from normal: candidates for a novel fault or a rule gap, for human
//! review (it never actuates — the principle).
//!
//! Usage: `sovereign-anomaly [log] [--top N]`.

use sovds::anomaly::{score, AnomalyConfig};
use sovds::domain::Domain;
use sovereign_adapters::dataset;
use sovereign_core::failure::FailureClass;
use sovereign_core::routerdomain::RouterDomain;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(default_log_path);
    let top = flag_value(&args, "--top")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10);

    let generations = std::env::var("SOVEREIGN_LOG_GENERATIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);
    let corpus = dataset::load(&log, generations);
    if corpus.records.is_empty() {
        eprintln!("sovereign-anomaly: no records at {}", log.display());
        return ExitCode::FAILURE;
    }

    let scores = score::<RouterDomain>(&corpus.records, AnomalyConfig::new());

    // Distribution.
    let mut sorted = scores.clone();
    sorted.sort_unstable();
    let p = |q: usize| percentile(&sorted, q);
    println!(
        "Anomaly score distribution (n={}): p50={} p90={} p99={} max={}",
        scores.len(),
        p(50),
        p(90),
        p(99),
        sorted.last().copied().unwrap_or(0),
    );

    // Threshold: the p99 line, but at least one typical deviation, so a uniform
    // corpus does not flag noise.
    let threshold = p(99).max(1000);
    println!("\nFlagging records with score >= {threshold} (top ~1%, at least ~1 deviation):\n");

    // Split flagged records by whether the rules already caught them.
    let mut caught = 0usize;
    let mut missed: Vec<(usize, u64)> = Vec::new();
    for (i, &s) in scores.iter().enumerate() {
        if s < threshold {
            continue;
        }
        let is_none = corpus
            .records
            .get(i)
            .is_some_and(|r| r.class == FailureClass::None);
        if is_none {
            missed.push((i, s));
        } else {
            caught = caught.saturating_add(1);
        }
    }
    missed.sort_by_key(|&(_, s)| std::cmp::Reverse(s));

    println!(
        "  {} flagged already had a fault class (rules caught them).",
        caught
    );
    println!(
        "  {} flagged were class=none — rule-gap candidates (shown below).\n",
        missed.len()
    );

    if missed.is_empty() {
        println!("  None: no healthy-labelled record looks anomalous. The rules are not");
        println!("  visibly missing anything in this corpus.");
    } else {
        for &(i, s) in missed.iter().take(top) {
            if let Some(r) = corpus.records.get(i) {
                println!(
                    "  score={s:<7} ts={:<11} {}",
                    r.wall_time,
                    signals_line(&r.obs)
                );
            }
        }
    }

    eprintln!(
        "\n(scored {} record(s); {} skipped)",
        corpus.records.len(),
        corpus.skipped
    );
    ExitCode::SUCCESS
}

/// Render a record's raw signals as `name=value` pairs for human context.
fn signals_line(obs: &sovereign_core::domain::Telemetry) -> String {
    let names = RouterDomain::signal_names();
    let mut out = String::new();
    let mut idx = 0usize;
    RouterDomain::signals(obs, &mut |v| {
        if let Some(name) = names.get(idx) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("{name}={v}"));
        }
        idx = idx.saturating_add(1);
    });
    out
}

/// Nearest-rank percentile of a sorted slice.
fn percentile(sorted: &[u64], q: usize) -> u64 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    let rank = q
        .saturating_mul(n)
        .saturating_add(99)
        .checked_div(100)
        .unwrap_or(0);
    let idx = rank.saturating_sub(1).min(n.saturating_sub(1));
    sorted.get(idx).copied().unwrap_or(0)
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i.saturating_add(1)))
        .map(String::as_str)
}

fn default_log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}
