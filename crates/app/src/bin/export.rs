// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `sovereign-export` — flatten the collected dataset into a TSV feature table.
//!
//! The record log is self-describing but not rectangular. This turns it into a flat
//! `t ts probed class inject <signals…>` table that pandas, DuckDB, or a training
//! pipeline can read directly — the structured, model-ready form of the dataset,
//! with zero feature engineering (raw signals only).
//!
//! Usage: `sovereign-export [log] [out.tsv]` (defaults: XDG log → stdout).

use sovds::domain::Domain;
use sovds::export::{write_header, write_row};
use sovereign_adapters::dataset;
use sovereign_core::routerdomain::RouterDomain;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(default_log_path);
    let out = args.get(1).map(PathBuf::from);

    let generations = std::env::var("SOVEREIGN_LOG_GENERATIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);
    let corpus = dataset::load(&log, generations);
    if corpus.records.is_empty() {
        eprintln!("sovereign-export: no records at {}", log.display());
        return ExitCode::FAILURE;
    }

    // Build the whole table in memory, then write it in one shot.
    let mut table = String::new();
    let _ = write_header::<RouterDomain, _>(&mut table);
    for r in &corpus.records {
        let _ = write_row(&mut table, r);
    }

    let written = match &out {
        Some(path) => std::fs::write(path, table.as_bytes())
            .map(|()| format!("→ {}", path.display()))
            .map_err(|e| format!("write failed: {e}")),
        None => std::io::stdout()
            .write_all(table.as_bytes())
            .map(|()| "→ stdout".to_string())
            .map_err(|e| format!("write failed: {e}")),
    };
    match written {
        Ok(dest) => {
            // Framing columns (t ts probed class inject) + the domain's signals.
            let columns = 5usize.saturating_add(RouterDomain::signal_names().len());
            eprintln!(
                "sovereign-export: {} record(s), {columns} column(s) {dest} ({} skipped)",
                corpus.records.len(),
                corpus.skipped
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("sovereign-export: {err}");
            ExitCode::FAILURE
        }
    }
}

fn default_log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}
