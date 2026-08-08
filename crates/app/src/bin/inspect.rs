// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `sovereign-inspect` — read the collected record log and print dataset metrics.
//!
//! This is the "metrics before theories" instrument (the principle): it turns the
//! accumulating trail into the numbers that decide the next move — class balance
//! (is a model even warranted?), real latency distributions (tune the adaptive
//! thresholds from data, not guesses), and per-node availability.

use sovereign_adapters::dataset;
use sovereign_adapters::summary::summarize;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = log_path();
    let generations = std::env::var("SOVEREIGN_LOG_GENERATIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);

    let corpus = dataset::load(&path, generations);
    if corpus.records.is_empty() {
        eprintln!(
            "sovereign-inspect: no records found at {} ({} file(s) read, {} line(s) skipped)",
            path.display(),
            corpus.files,
            corpus.skipped
        );
        return ExitCode::FAILURE;
    }

    print!("{}", summarize(&corpus.records));
    eprintln!(
        "\n(loaded {} record(s) from {} file(s); {} unparseable line(s) skipped)",
        corpus.records.len(),
        corpus.files,
        corpus.skipped
    );
    ExitCode::SUCCESS
}

/// Resolve the log path: first CLI argument, else `$SOVEREIGN_LOG`, else the
/// default XDG data location on the laptop.
fn log_path() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
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
